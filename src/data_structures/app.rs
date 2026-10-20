use std::collections::BTreeSet;
use std::io::{self, Read};
use std::process::Child;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, TableState, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::data_structures::{
    COMPLETION_KINDS, Cheatsheet, Completer, Journal, KubeTable, Msg, RunMode, Target, Theme, Viewer, ViewerAction,
};
use crate::helpers::{self, argv};

const WATCH_INTERVAL: Duration = Duration::from_secs(2);
const ALL_NAMESPACES: &str = "(all namespaces)";
const COMPLETION_TTL: Duration = Duration::from_secs(15);
const JOURNAL_STRIP_HEIGHT: u16 = 7;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Contexts,
    Resources,
    Helm,
    Flux,
    Journal,
    Cheatsheet,
}

impl Tab {
    const ALL: [Tab; 6] = [Tab::Contexts, Tab::Resources, Tab::Helm, Tab::Flux, Tab::Journal, Tab::Cheatsheet];

    fn title(self) -> &'static str {
        match self {
            Tab::Contexts => "1 Contexts",
            Tab::Resources => "2 Resources",
            Tab::Helm => "3 Helm",
            Tab::Flux => "4 Flux",
            Tab::Journal => "5 Journal",
            Tab::Cheatsheet => "6 Cheatsheet",
        }
    }

    /// Short name used in journal origins ("Resources ▸ delete pods/web").
    fn name(self) -> &'static str {
        &self.title()[2..]
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }
}

/// What a prompt's input is for once Enter is pressed.
enum Prompt {
    Scale { target: String, namespace: Option<String> },
    PortForward { target: String, namespace: Option<String> },
    Rollback { release: String, namespace: Option<String> },
}

enum Mode {
    Normal,
    Filter,
    Command,
    Prompt(Prompt),
    Confirm { message: String, argv: Vec<String> },
    Namespaces,
    PortForwards,
    Help,
    /// Waiting for the key after the leader (Space).
    Leader,
}

/// Leader-key bindings: Space + key opens a view.
const LEADER_KEYS: &[(char, &str)] = &[
    ('p', "pods"),
    ('d', "deployments"),
    ('s', "services"),
    ('i', "ingresses"),
    ('t', "statefulsets"),
    ('D', "daemonsets"),
    ('j', "jobs"),
    ('J', "cronjobs"),
    ('c', "configmaps"),
    ('S', "secrets"),
    ('v', "persistentvolumeclaims"),
    ('e', "events"),
    ('N', "nodes"),
    ('h', "helm releases"),
    ('F', "flux"),
    ('l', "journal (commands run)"),
    ('x', "contexts"),
    ('n', "pick namespace"),
    ('a', "all namespaces ⇄ current"),
    ('?', "cheatsheet"),
    (':', "command prompt"),
    ('f', "port-forwards"),
];

struct ResourceKind {
    name: &'static str,
    namespaced: bool,
    logs: bool,
    exec: bool,
    restart: bool,
    scale: bool,
    forward: bool,
}

const NAMESPACED: ResourceKind = ResourceKind {
    name: "",
    namespaced: true,
    logs: false,
    exec: false,
    restart: false,
    scale: false,
    forward: false,
};

const KINDS: &[ResourceKind] = &[
    ResourceKind { name: "pods", logs: true, exec: true, forward: true, ..NAMESPACED },
    ResourceKind { name: "deployments", logs: true, restart: true, scale: true, forward: true, ..NAMESPACED },
    ResourceKind { name: "statefulsets", logs: true, restart: true, scale: true, forward: true, ..NAMESPACED },
    ResourceKind { name: "daemonsets", logs: true, restart: true, ..NAMESPACED },
    ResourceKind { name: "replicasets", scale: true, ..NAMESPACED },
    ResourceKind { name: "jobs", logs: true, ..NAMESPACED },
    ResourceKind { name: "cronjobs", ..NAMESPACED },
    ResourceKind { name: "services", forward: true, ..NAMESPACED },
    ResourceKind { name: "ingresses", ..NAMESPACED },
    ResourceKind { name: "configmaps", ..NAMESPACED },
    ResourceKind { name: "secrets", ..NAMESPACED },
    ResourceKind { name: "persistentvolumeclaims", ..NAMESPACED },
    ResourceKind { name: "horizontalpodautoscalers", ..NAMESPACED },
    ResourceKind { name: "serviceaccounts", ..NAMESPACED },
    ResourceKind { name: "events", ..NAMESPACED },
    ResourceKind { name: "nodes", namespaced: false, ..NAMESPACED },
    ResourceKind { name: "namespaces", namespaced: false, ..NAMESPACED },
    ResourceKind { name: "persistentvolumes", namespaced: false, ..NAMESPACED },
    ResourceKind { name: "storageclasses", namespaced: false, ..NAMESPACED },
];

/// A Flux object type and how each flux CLI subcommand spells it.
struct FluxKind {
    label: &'static str,
    /// `flux get <get...>`
    get: &'static [&'static str],
    /// `flux suspend|resume|reconcile <cli...> NAME`
    cli: &'static [&'static str],
    /// Full CRD name for kubectl describe / get -o yaml.
    crd: &'static str,
    /// Kind as used by `flux events --for` and `flux logs --kind`.
    kind: &'static str,
    /// reconcile supports --with-source (fetch git/chart first).
    with_source: bool,
    /// `flux tree` is supported.
    tree: bool,
}

const FLUX_KINDS: &[FluxKind] = &[
    FluxKind {
        label: "kustomizations",
        get: &["kustomizations"],
        cli: &["kustomization"],
        crd: "kustomizations.kustomize.toolkit.fluxcd.io",
        kind: "Kustomization",
        with_source: true,
        tree: true,
    },
    FluxKind {
        label: "helmreleases",
        get: &["helmreleases"],
        cli: &["helmrelease"],
        crd: "helmreleases.helm.toolkit.fluxcd.io",
        kind: "HelmRelease",
        with_source: true,
        tree: false,
    },
    FluxKind {
        label: "git sources",
        get: &["sources", "git"],
        cli: &["source", "git"],
        crd: "gitrepositories.source.toolkit.fluxcd.io",
        kind: "GitRepository",
        with_source: false,
        tree: false,
    },
    FluxKind {
        label: "helm repositories",
        get: &["sources", "helm"],
        cli: &["source", "helm"],
        crd: "helmrepositories.source.toolkit.fluxcd.io",
        kind: "HelmRepository",
        with_source: false,
        tree: false,
    },
    FluxKind {
        label: "oci sources",
        get: &["sources", "oci"],
        cli: &["source", "oci"],
        crd: "ocirepositories.source.toolkit.fluxcd.io",
        kind: "OCIRepository",
        with_source: false,
        tree: false,
    },
    FluxKind {
        label: "helm charts",
        get: &["sources", "chart"],
        cli: &["source", "chart"],
        crd: "helmcharts.source.toolkit.fluxcd.io",
        kind: "HelmChart",
        with_source: false,
        tree: false,
    },
];

/// A `kubectl port-forward` running in the background for as long as the app runs.
struct PortForward {
    label: String,
    child: Child,
    journal: u64,
}

/// A command waiting to take over the real terminal (exec -it, edit, `!cmd`).
struct Interactive {
    argv: Vec<String>,
    pause: bool,
    journal: u64,
}

/// Selection + filter state shared by the table-based tabs.
#[derive(Default)]
struct TablePane {
    table: KubeTable,
    state: TableState,
    filter: String,
    loading: bool,
    error: Option<String>,
    command: String,
    request: u64,
    last_refresh: Option<Instant>,
}

impl TablePane {
    fn visible(&self) -> Vec<usize> {
        self.table.visible_rows(&self.filter)
    }

    fn selected_row(&self) -> Option<&[String]> {
        let visible = self.visible();
        self.state
            .selected()
            .and_then(|i| visible.get(i))
            .map(|&i| self.table.rows[i].as_slice())
    }

    fn step(&mut self, delta: isize) {
        let len = self.visible().len();
        self.state.select(helpers::step_selection(self.state.selected(), len, delta));
    }

    fn clamp(&mut self) {
        let len = self.visible().len();
        self.state.select(helpers::clamp_selection(self.state.selected(), len));
    }

    fn set_result(&mut self, result: Result<KubeTable, String>) {
        self.loading = false;
        self.last_refresh = Some(Instant::now());
        match result {
            Ok(table) => {
                self.table = table;
                self.error = None;
            }
            Err(e) => self.error = Some(e.trim().to_string()),
        }
        self.clamp();
    }

    fn reset(&mut self) {
        self.table = KubeTable::default();
        self.error = None;
        self.state.select(Some(0));
    }

    fn watch_due(&self) -> bool {
        !self.loading && self.last_refresh.is_none_or(|t| t.elapsed() >= WATCH_INTERVAL)
    }

    fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        title: String,
        theme: &Theme,
        row_style: impl Fn(&[String]) -> Option<Style>,
    ) {
        let visible = self.visible();
        let mut title_spans = vec![Span::styled(title, Style::new().bold())];
        title_spans.push(Span::raw(format!(" ({}) ", visible.len())));
        if !self.filter.is_empty() {
            title_spans.push(Span::styled(format!("/{} ", self.filter), theme.fg(theme.warning)));
        }
        if self.loading {
            title_spans.push(Span::styled(" loading… ", theme.badge(theme.warning)));
        }
        let block = theme
            .block()
            .title(Line::from(title_spans))
            .title_bottom(Line::from(format!(" $ {} ", self.command)).style(theme.fg(theme.muted)));

        match &self.error {
            Some(error) if self.table.rows.is_empty() => {
                let text = Paragraph::new(error.as_str())
                    .style(theme.fg(theme.danger))
                    .wrap(Wrap { trim: false })
                    .block(block);
                frame.render_widget(text, area);
            }
            _ if self.table.headers.is_empty() && !self.loading => {
                frame.render_widget(Paragraph::new("No resources found.").style(theme.fg(theme.muted)).block(block), area);
            }
            _ => self.table.render(frame, area, &visible, &mut self.state, block, theme, row_style),
        }
    }
}

pub struct App {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    running: bool,
    tab: Tab,
    mode: Mode,
    watch: bool,

    contexts: TablePane,
    current_context: String,
    namespace: Option<String>,
    previous_namespace: Option<String>,
    namespace_initialized: bool,
    namespaces: Vec<String>,
    namespace_state: ListState,
    namespace_filter: String,

    kind_index: usize,
    resources: TablePane,
    helm: TablePane,
    flux_kind_index: usize,
    flux: TablePane,
    cheatsheet: Cheatsheet,
    journal: Journal,
    show_journal_strip: bool,
    themes: Vec<Theme>,
    theme_index: usize,

    viewer: Option<Viewer>,
    next_generation: u64,
    port_forwards: Vec<PortForward>,
    port_forward_state: ListState,

    input: String,
    history: Vec<String>,
    history_index: Option<usize>,
    completer: Completer,
    completions_fetched: Option<Instant>,
    status: Option<(String, bool)>,
    pending_interactive: Option<Interactive>,
}

impl App {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let (themes, theme_index, theme_problems) = Theme::load_all();
        let status = theme_problems.first().map(|p| (format!("✘ theme: {p}"), true));
        Self {
            tx,
            rx,
            running: true,
            tab: Tab::Resources,
            mode: Mode::Normal,
            watch: false,
            contexts: TablePane::default(),
            current_context: String::new(),
            namespace: None,
            previous_namespace: None,
            namespace_initialized: false,
            namespaces: Vec::new(),
            namespace_state: ListState::default(),
            namespace_filter: String::new(),
            kind_index: 0,
            resources: TablePane::default(),
            helm: TablePane::default(),
            flux_kind_index: 0,
            flux: TablePane::default(),
            cheatsheet: Cheatsheet::new(),
            journal: Journal::open(),
            show_journal_strip: true,
            themes,
            theme_index,
            viewer: None,
            next_generation: 0,
            port_forwards: Vec::new(),
            port_forward_state: ListState::default(),
            input: String::new(),
            history: Vec::new(),
            history_index: None,
            completer: Completer::default(),
            completions_fetched: None,
            status,
            pending_interactive: None,
        }
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        // Resources, helm and flux are loaded once the context's namespace is known.
        self.refresh_contexts(false);
        self.refresh_namespaces();
        while self.running {
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.handle_key(key);
            }
            while let Ok(msg) = self.rx.try_recv() {
                self.handle_msg(msg);
            }
            self.tick();
            if let Some(job) = self.pending_interactive.take() {
                let outcome = helpers::run_interactive(terminal, &job.argv, job.pause)?;
                match outcome {
                    Ok(()) => self.journal.finish(job.journal, true, "returned from terminal"),
                    Err(e) => {
                        self.journal.finish(job.journal, false, &e);
                        self.set_error(e);
                    }
                }
                self.refresh_current(true);
            }
        }
        if let Some(viewer) = self.viewer.take() {
            self.journal.finish(viewer.journal, true, "stopped (quit)");
        }
        for pf in &mut self.port_forwards {
            let _ = pf.child.kill();
            let _ = pf.child.wait();
            self.journal.finish(pf.journal, true, "stopped (quit)");
        }
        Ok(())
    }

    // ── background results ────────────────────────────────────────────────

    fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Output { target, journal, result } => {
                match &result {
                    Ok(out) => self.journal.finish(journal, true, out),
                    Err(e) => self.journal.finish(journal, false, e),
                }
                self.on_output(target, result);
            }
            Msg::Line { generation, line } => {
                if let Some(viewer) = self.viewer.as_mut().filter(|v| v.generation == generation) {
                    viewer.push_line(&line);
                }
            }
            Msg::StreamEnd { generation } => {
                if let Some(viewer) = self.viewer.as_mut().filter(|v| v.generation == generation) {
                    viewer.end_stream();
                    self.journal.finish(viewer.journal, true, "stream ended");
                }
            }
        }
    }

    fn on_output(&mut self, target: Target, result: Result<String, String>) {
        match target {
            Target::Contexts => self.on_contexts(result),
            Target::Namespaces => match result {
                Ok(text) => {
                    self.namespaces = text.split_whitespace().map(String::from).collect();
                    self.namespaces.sort();
                    self.completer.namespaces = self.namespaces.clone();
                    self.clamp_namespace_selection();
                }
                Err(e) => self.set_error(e),
            },
            Target::Resources(id) if id == self.resources.request => {
                self.resources.set_result(result.map(|t| KubeTable::parse(&t)));
            }
            Target::Helm(id) if id == self.helm.request => {
                self.helm.set_result(result.and_then(|t| KubeTable::from_helm_json(&t)));
                self.completer.releases = Self::column_values(&self.helm.table, "NAME");
            }
            Target::Flux(id) if id == self.flux.request => {
                self.flux.set_result(result.map(|t| KubeTable::parse(&t)));
                self.completer.flux_names = Self::column_values(&self.flux.table, "NAME");
            }
            Target::Resources(_) | Target::Helm(_) | Target::Flux(_) => {}
            Target::Completions => {
                if let Ok(text) = result {
                    self.completer.set_resources(&text);
                }
            }
            Target::Viewer(generation) => {
                if let Some(viewer) = self.viewer.as_mut().filter(|v| v.generation == generation) {
                    viewer.set_output(result);
                }
            }
            Target::Action => {
                match result {
                    Ok(out) => {
                        let first = out.lines().find(|l| !l.trim().is_empty()).unwrap_or("done");
                        self.set_status(format!("✔ {first}"));
                    }
                    Err(e) => self.set_error(e),
                }
                self.refresh_current(true);
            }
        }
    }

    fn on_contexts(&mut self, result: Result<String, String>) {
        let parsed = result.map(|text| KubeTable::parse(&text));
        let mut context_namespace = None;
        if let Ok(table) = &parsed
            && let Some(row) = table.rows.iter().find(|r| table.cell(r, "CURRENT") == Some("*"))
        {
            self.current_context = table.cell(row, "NAME").unwrap_or_default().to_string();
            context_namespace = table.cell(row, "NAMESPACE").map(String::from);
        }
        self.contexts.set_result(parsed);
        self.completer.contexts = Self::column_values(&self.contexts.table, "NAME");

        if !self.namespace_initialized {
            // Same default kubectl itself uses when the context sets no namespace.
            self.namespace = Some(context_namespace.unwrap_or_else(|| "default".into()));
            self.namespace_initialized = true;
            self.refresh_resources(false);
            self.refresh_helm(false);
            self.refresh_flux(false);
        }
    }

    fn column_values(table: &KubeTable, column: &str) -> Vec<String> {
        table.rows.iter().filter_map(|r| table.cell(r, column)).map(String::from).collect()
    }

    /// Periodic work: auto-refresh in watch mode and reap dead port-forwards.
    fn tick(&mut self) {
        if self.viewer.as_ref().is_some_and(Viewer::watch_due) {
            self.rerun_viewer(true);
        }
        if self.watch && self.viewer.is_none() {
            match self.tab {
                Tab::Resources if self.resources.watch_due() => self.refresh_resources(true),
                Tab::Helm if self.helm.watch_due() => self.refresh_helm(true),
                Tab::Flux if self.flux.watch_due() => self.refresh_flux(true),
                Tab::Contexts if self.contexts.watch_due() => self.refresh_contexts(true),
                _ => {}
            }
        }

        let mut exited = Vec::new();
        self.port_forwards.retain_mut(|pf| match pf.child.try_wait() {
            Ok(None) => true,
            _ => {
                let mut stderr = String::new();
                if let Some(mut pipe) = pf.child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                exited.push((pf.journal, format!("port-forward {} exited: {}", pf.label, stderr.trim())));
                false
            }
        });
        for (journal, message) in exited {
            self.journal.finish(journal, false, &message);
            self.set_error(message);
        }
    }

    // ── running commands (everything goes through the journal) ───────────

    /// Runs a command in the background; the result arrives as `Msg::Output` for `target`.
    /// `auto` marks commands the tool issues by itself (polling, follow-up refreshes).
    fn capture(&mut self, argv: Vec<String>, target: Target, origin: &str, auto: bool) {
        let journal = self.journal.record(&argv, origin, RunMode::Capture, auto);
        helpers::spawn_capture(argv, target, journal, self.tx.clone());
    }

    /// Hands the real terminal to a command after this frame.
    fn queue_interactive(&mut self, argv: Vec<String>, pause: bool, origin: &str) {
        let journal = self.journal.record(&argv, origin, RunMode::Interactive, false);
        self.pending_interactive = Some(Interactive { argv, pause, journal });
    }

    fn kind(&self) -> &'static ResourceKind {
        &KINDS[self.kind_index]
    }

    fn flux_kind(&self) -> &'static FluxKind {
        &FLUX_KINDS[self.flux_kind_index]
    }

    /// `-n <ns>` or `-A` depending on the namespace scope.
    fn scope_args(&self) -> Vec<String> {
        match &self.namespace {
            Some(ns) => argv(&["-n", ns]),
            None => argv(&["-A"]),
        }
    }

    fn refresh_contexts(&mut self, auto: bool) {
        let argv = argv(&["kubectl", "config", "get-contexts"]);
        self.contexts.command = helpers::shell_join(&argv);
        self.contexts.loading = true;
        self.capture(argv, Target::Contexts, "Contexts ▸ list contexts", auto);
    }

    fn refresh_namespaces(&mut self) {
        let argv = argv(&["kubectl", "get", "namespaces", "-o", "jsonpath={.items[*].metadata.name}"]);
        self.capture(argv, Target::Namespaces, "list namespaces (picker/completion)", true);
    }

    fn refresh_resources(&mut self, auto: bool) {
        let kind = self.kind();
        let mut argv = argv(&["kubectl", "get", kind.name]);
        if kind.namespaced {
            argv.extend(self.scope_args());
        }
        if kind.name == "events" {
            argv.push("--sort-by=.lastTimestamp".into());
        } else {
            argv.extend(["-o".into(), "wide".into()]);
        }
        self.resources.request += 1;
        self.resources.loading = true;
        self.resources.command = helpers::shell_join(&argv);
        let target = Target::Resources(self.resources.request);
        self.capture(argv, target, &format!("Resources ▸ list {}", kind.name), auto);
    }

    fn refresh_helm(&mut self, auto: bool) {
        let mut argv = argv(&["helm", "list", "--all", "-o", "json"]);
        argv.extend(self.scope_args());
        self.helm.request += 1;
        self.helm.loading = true;
        self.helm.command = helpers::shell_join(&argv);
        let target = Target::Helm(self.helm.request);
        self.capture(argv, target, "Helm ▸ list releases", auto);
    }

    /// Flux objects usually live in flux-system, so this view is always cluster-wide.
    fn refresh_flux(&mut self, auto: bool) {
        let kind = self.flux_kind();
        let mut argv = argv(&["flux", "get"]);
        argv.extend(kind.get.iter().map(|s| s.to_string()));
        argv.push("-A".into());
        self.flux.request += 1;
        self.flux.loading = true;
        self.flux.command = helpers::shell_join(&argv);
        let target = Target::Flux(self.flux.request);
        self.capture(argv, target, &format!("Flux ▸ list {}", kind.label), auto);
    }

    fn refresh_current(&mut self, auto: bool) {
        match self.tab {
            Tab::Contexts => self.refresh_contexts(auto),
            Tab::Resources => self.refresh_resources(auto),
            Tab::Helm => self.refresh_helm(auto),
            Tab::Flux => self.refresh_flux(auto),
            Tab::Journal | Tab::Cheatsheet => {}
        }
    }

    /// A one-shot change (delete, scale, suspend…); result goes to the status bar.
    fn run_action(&mut self, argv: Vec<String>, origin: &str) {
        self.set_status(format!("$ {}", helpers::shell_join(&argv)));
        self.capture(argv, Target::Action, origin, false);
    }

    /// Shows the exact command and asks before running it. `e` loads it into the prompt instead.
    fn confirm(&mut self, message: String, argv: Vec<String>) {
        self.mode = Mode::Confirm { message, argv };
    }

    fn open_viewer(&mut self, title: String, argv: Vec<String>, stream: bool, watch: Option<Duration>) {
        self.close_viewer();
        self.next_generation += 1;
        let generation = self.next_generation;
        let origin = format!("{} ▸ {title}", self.tab.name());
        let mut viewer = Viewer::new(title, argv, generation);
        viewer.watch = watch;
        if stream {
            viewer.journal = self.journal.record(&viewer.argv, &origin, RunMode::Stream, false);
            match helpers::spawn_stream(&viewer.argv, generation, &self.tx) {
                Ok(child) => viewer.attach_stream(child),
                Err(e) => {
                    let message = format!("failed to start: {e}");
                    self.journal.finish(viewer.journal, false, &message);
                    viewer.set_output(Err(message));
                }
            }
        } else {
            viewer.journal = self.journal.record(&viewer.argv, &origin, RunMode::Capture, false);
            viewer.mark_running();
            helpers::spawn_capture(viewer.argv.clone(), Target::Viewer(generation), viewer.journal, self.tx.clone());
        }
        self.viewer = Some(viewer);
    }

    fn close_viewer(&mut self) {
        if let Some(viewer) = self.viewer.take() {
            // Only matters for streams still running; finished entries are left alone.
            self.journal.finish(viewer.journal, true, "stopped by you");
        }
    }

    fn rerun_viewer(&mut self, auto: bool) {
        if let Some(viewer) = self.viewer.as_mut() {
            let origin = if auto { format!("watch ▸ {}", viewer.title) } else { format!("rerun ▸ {}", viewer.title) };
            viewer.journal = self.journal.record(&viewer.argv, &origin, RunMode::Capture, auto);
            viewer.mark_running();
            helpers::spawn_capture(viewer.argv.clone(), Target::Viewer(viewer.generation), viewer.journal, self.tx.clone());
        }
    }

    /// Opens the `:` prompt, refreshing the completion data if it is stale.
    fn open_command(&mut self, initial: String) {
        self.input = initial;
        self.history_index = None;
        self.completer.reset();
        self.mode = Mode::Command;
        if self.completions_fetched.is_none_or(|t| t.elapsed() >= COMPLETION_TTL) {
            self.completions_fetched = Some(Instant::now());
            let mut argv = argv(&["kubectl", "get", COMPLETION_KINDS, "-o", "name"]);
            argv.extend(self.scope_args());
            self.capture(argv, Target::Completions, "load names for Tab completion", true);
            if self.namespaces.is_empty() {
                self.refresh_namespaces();
            }
        }
    }

    /// Runs a line typed into the `:` prompt.
    /// `!cmd` runs on the real terminal, `watch [-n N] cmd` re-runs periodically,
    /// commands with -f/-w stream live, everything else is captured once.
    fn run_command_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if self.history.last().is_none_or(|last| last != line) {
            self.history.push(line.to_string());
        }

        if let Some(rest) = line.strip_prefix('!') {
            let argv = helpers::shell(&helpers::expand_alias(rest.trim()));
            self.queue_interactive(argv, true, "typed at : (terminal)");
            return;
        }
        let expanded = helpers::expand_alias(line);
        let line = expanded.as_str();
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let interactive = (tokens.first() == Some(&"kubectl") && tokens.contains(&"edit"))
            || tokens.iter().any(|t| matches!(*t, "-it" | "-ti" | "--tty" | "-i" | "--stdin"));
        if interactive {
            self.queue_interactive(helpers::shell(line), false, "typed at : (terminal)");
            return;
        }

        if tokens.first() == Some(&"watch") {
            let (interval, skip) = match (tokens.get(1), tokens.get(2).and_then(|n| n.parse::<f64>().ok())) {
                (Some(&"-n"), Some(secs)) => (Duration::from_secs_f64(secs.max(0.5)), 3),
                _ => (WATCH_INTERVAL, 1),
            };
            let command = tokens[skip..].join(" ");
            if command.is_empty() {
                self.set_error("usage: watch [-n SECONDS] <command>".into());
                return;
            }
            self.open_viewer(format!("typed: watch {command}"), helpers::shell(&command), false, Some(interval));
            return;
        }

        let stream = tokens.iter().any(|t| matches!(*t, "-f" | "--follow" | "-w" | "--watch"));
        self.open_viewer(format!("typed: {line}"), helpers::shell(line), stream, None);
    }

    // ── key handling ──────────────────────────────────────────────────────

    fn handle_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.running = false;
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Filter => self.handle_filter_key(key),
            Mode::Command => self.handle_command_key(key),
            Mode::Prompt(prompt) => self.handle_prompt_key(key, prompt),
            Mode::Confirm { message, argv } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let origin = format!("{} ▸ {}", self.tab.name(), message.trim_end_matches('?'));
                    self.run_action(argv, &origin);
                }
                KeyCode::Char('e') => self.open_command(helpers::shell_join(&argv)),
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => self.set_status("cancelled".into()),
                _ => self.mode = Mode::Confirm { message, argv },
            },
            Mode::Namespaces => self.handle_namespace_key(key),
            Mode::PortForwards => self.handle_port_forward_key(key),
            Mode::Help => {
                if !matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')) {
                    self.mode = Mode::Help;
                }
            }
            Mode::Leader => self.handle_leader_key(key),
        }
    }

    fn handle_leader_key(&mut self, key: KeyEvent) {
        let KeyCode::Char(c) = key.code else { return };
        match c {
            'h' => self.tab = Tab::Helm,
            'F' => self.tab = Tab::Flux,
            'l' => self.tab = Tab::Journal,
            'x' => self.tab = Tab::Contexts,
            '?' => self.tab = Tab::Cheatsheet,
            ':' => self.open_command(String::new()),
            'n' => self.open_namespace_picker(),
            'f' => self.open_port_forwards(),
            'a' => {
                let next = match self.namespace.take() {
                    Some(current) => {
                        self.previous_namespace = Some(current);
                        None
                    }
                    None => Some(self.previous_namespace.clone().unwrap_or_else(|| "default".into())),
                };
                self.set_namespace(next);
                self.set_status(format!("namespace: {}", self.namespace.as_deref().unwrap_or(ALL_NAMESPACES)));
            }
            _ => {
                let Some((_, kind)) = LEADER_KEYS.iter().find(|(k, _)| *k == c) else { return };
                if let Some(index) = KINDS.iter().position(|k| k.name == *kind) {
                    self.tab = Tab::Resources;
                    self.select_kind(index as isize - self.kind_index as isize);
                }
            }
        }
    }

    fn open_namespace_picker(&mut self) {
        self.namespace_filter.clear();
        self.namespace_state.select(Some(0));
        self.refresh_namespaces();
        self.mode = Mode::Namespaces;
    }

    fn open_port_forwards(&mut self) {
        self.port_forward_state.select(helpers::clamp_selection(None, self.port_forwards.len()));
        self.mode = Mode::PortForwards;
    }

    fn set_namespace(&mut self, namespace: Option<String>) {
        self.namespace = namespace;
        self.resources.reset();
        self.helm.reset();
        self.completions_fetched = None;
        self.refresh_resources(false);
        self.refresh_helm(false);
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        if let Some(viewer) = self.viewer.as_mut() {
            match viewer.handle_key(key) {
                ViewerAction::Close => self.close_viewer(),
                ViewerAction::Rerun => self.rerun_viewer(false),
                ViewerAction::None => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('q') => self.running = false,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char(' ') => self.mode = Mode::Leader,
            KeyCode::Char(':') => self.open_command(String::new()),
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char(c @ '1'..='6') => self.tab = Tab::ALL[c as usize - '1' as usize],
            KeyCode::Tab => self.tab = Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()],
            KeyCode::BackTab => self.tab = Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()],
            KeyCode::Char('n') => self.open_namespace_picker(),
            KeyCode::Char('w') => {
                self.watch = !self.watch;
                self.set_status(format!("auto-refresh every {}s {}", WATCH_INTERVAL.as_secs(), if self.watch { "on" } else { "off" }));
            }
            KeyCode::Char('J') => self.show_journal_strip = !self.show_journal_strip,
            KeyCode::Char('T') => self.next_theme(),
            KeyCode::Char('R') | KeyCode::F(5) => self.refresh_current(false),
            KeyCode::Char('P') => self.open_port_forwards(),
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::PageDown => self.step(15),
            KeyCode::PageUp => self.step(-15),
            KeyCode::Home | KeyCode::Char('g') => self.step(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.step(isize::MAX / 2),
            KeyCode::Esc => self.status = None,
            _ => match self.tab {
                Tab::Contexts => self.handle_contexts_key(key),
                Tab::Resources => self.handle_resources_key(key),
                Tab::Helm => self.handle_helm_key(key),
                Tab::Flux => self.handle_flux_key(key),
                Tab::Journal => self.handle_journal_key(key),
                Tab::Cheatsheet => self.handle_cheatsheet_key(key),
            },
        }
    }

    fn step(&mut self, delta: isize) {
        match self.tab {
            Tab::Contexts => self.contexts.step(delta),
            Tab::Resources => self.resources.step(delta),
            Tab::Helm => self.helm.step(delta),
            Tab::Flux => self.flux.step(delta),
            Tab::Journal => self.journal.step(delta),
            Tab::Cheatsheet => self.cheatsheet.step(delta),
        }
    }

    fn handle_contexts_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let Some(name) = self.contexts.selected_row().and_then(|r| self.contexts.table.cell(r, "NAME")) else {
                    return;
                };
                let name = name.to_string();
                let argv = argv(&["kubectl", "config", "use-context", &name]);
                // The refresh after the switch re-reads the new context's namespace.
                self.namespace_initialized = false;
                self.resources.reset();
                self.helm.reset();
                self.flux.reset();
                self.run_action(argv, &format!("Contexts ▸ switch to {name}"));
            }
            KeyCode::Char('i') => {
                self.open_viewer("cluster-info".into(), argv(&["kubectl", "cluster-info"]), false, None);
            }
            KeyCode::Char('v') => {
                self.open_viewer("version".into(), argv(&["kubectl", "version"]), false, None);
            }
            _ => {}
        }
    }

    /// Name and namespace of the selected resource row.
    fn selected_resource(&self) -> Option<(String, Option<String>)> {
        let row = self.resources.selected_row()?;
        let table = &self.resources.table;
        let name = table.cell(row, "NAME")?.to_string();
        let namespace = if self.kind().namespaced {
            table.cell(row, "NAMESPACE").map(String::from).or_else(|| self.namespace.clone())
        } else {
            None
        };
        Some((name, namespace))
    }

    fn with_namespace(mut argv: Vec<String>, namespace: &Option<String>) -> Vec<String> {
        if let Some(ns) = namespace {
            argv.extend(["-n".into(), ns.clone()]);
        }
        argv
    }

    fn handle_resources_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Left | KeyCode::Char('[') => return self.select_kind(-1),
            KeyCode::Right | KeyCode::Char(']') => return self.select_kind(1),
            _ => {}
        }
        let kind = self.kind();
        let Some((name, ns)) = self.selected_resource() else {
            if matches!(key.code, KeyCode::Char(_) | KeyCode::Enter) {
                self.set_error(format!("no {} selected", kind.name));
            }
            return;
        };
        let target = format!("{}/{}", kind.name, name);
        let unsupported = |app: &mut Self, what: &str| app.set_error(format!("{what} is not available for {}", kind.name));

        match key.code {
            KeyCode::Enter | KeyCode::Char('d') => {
                let argv = Self::with_namespace(argv(&["kubectl", "describe", kind.name, &name]), &ns);
                self.open_viewer(format!("describe {target}"), argv, false, None);
            }
            KeyCode::Char('y') => {
                let argv = Self::with_namespace(argv(&["kubectl", "get", kind.name, &name, "-o", "yaml"]), &ns);
                self.open_viewer(format!("yaml {target}"), argv, false, None);
            }
            KeyCode::Char('v') => {
                let selector = format!("involvedObject.name={name}");
                let argv = Self::with_namespace(
                    argv(&["kubectl", "get", "events", "--field-selector", &selector, "--sort-by=.lastTimestamp"]),
                    &ns,
                );
                self.open_viewer(format!("events {target}"), argv, false, None);
            }
            KeyCode::Char('l') if kind.logs => {
                let argv = Self::with_namespace(
                    argv(&["kubectl", "logs", "-f", "--tail=500", "--all-containers", "--prefix", &target]),
                    &ns,
                );
                self.open_viewer(format!("logs {target}"), argv, true, None);
            }
            KeyCode::Char('L') if kind.name == "pods" => {
                let argv = Self::with_namespace(
                    argv(&["kubectl", "logs", "--previous", "--tail=500", "--all-containers", "--prefix", &target]),
                    &ns,
                );
                self.open_viewer(format!("previous logs {target}"), argv, false, None);
            }
            KeyCode::Char('e') if kind.exec => {
                let shell = "command -v bash >/dev/null 2>&1 && exec bash || exec sh";
                let argv = Self::with_namespace(argv(&["kubectl", "exec", "-it", &name]), &ns)
                    .into_iter()
                    .chain(argv(&["--", "sh", "-c", shell]))
                    .collect();
                self.queue_interactive(argv, false, &format!("Resources ▸ shell into {target}"));
            }
            KeyCode::Char('E') => {
                let argv = Self::with_namespace(argv(&["kubectl", "edit", &target]), &ns);
                self.queue_interactive(argv, false, &format!("Resources ▸ edit {target}"));
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                let argv = Self::with_namespace(argv(&["kubectl", "delete", &target]), &ns);
                self.confirm(format!("Delete {target}?"), argv);
            }
            KeyCode::Char('r') if kind.restart => {
                let argv = Self::with_namespace(argv(&["kubectl", "rollout", "restart", &target]), &ns);
                self.confirm(format!("Rollout restart {target}?"), argv);
            }
            KeyCode::Char('o') if kind.restart => {
                let argv = Self::with_namespace(argv(&["kubectl", "rollout", "status", "-w", &target]), &ns);
                self.open_viewer(format!("rollout status {target}"), argv, true, None);
            }
            KeyCode::Char('s') if kind.scale => {
                // READY "2/3" -> prefill desired replicas "3"
                let row = self.resources.selected_row().unwrap_or_default();
                let replicas = self.resources.table.cell(row, "READY").and_then(|r| r.split('/').nth(1));
                self.input = replicas.or_else(|| self.resources.table.cell(row, "DESIRED")).unwrap_or("1").to_string();
                self.mode = Mode::Prompt(Prompt::Scale { target, namespace: ns });
            }
            KeyCode::Char('f') if kind.forward => {
                let row = self.resources.selected_row().unwrap_or_default();
                let table = &self.resources.table;
                let port = table
                    .cell(row, "PORT(S)")
                    .and_then(|p| p.split(['/', ':', ',']).next())
                    .filter(|p| p.parse::<u16>().is_ok());
                self.input = port.map(|p| format!("{p}:{p}")).unwrap_or_default();
                self.mode = Mode::Prompt(Prompt::PortForward { target, namespace: ns });
            }
            KeyCode::Char('l') => unsupported(self, "logs"),
            KeyCode::Char('e') => unsupported(self, "exec"),
            KeyCode::Char('r') | KeyCode::Char('o') => unsupported(self, "rollout"),
            KeyCode::Char('s') if kind.name == "pods" => self.set_error(
                "pods aren't scaled directly: scale the Deployment/StatefulSet that owns them (Space d, select it, s)".into(),
            ),
            KeyCode::Char('s') => unsupported(self, "scale"),
            KeyCode::Char('f') => unsupported(self, "port-forward"),
            _ => {}
        }
    }

    fn select_kind(&mut self, delta: isize) {
        let next = helpers::step_selection(Some(self.kind_index), KINDS.len(), delta).unwrap_or(0);
        if next != self.kind_index {
            self.kind_index = next;
            self.resources.reset();
            self.refresh_resources(false);
        }
    }

    fn select_flux_kind(&mut self, delta: isize) {
        let next = helpers::step_selection(Some(self.flux_kind_index), FLUX_KINDS.len(), delta).unwrap_or(0);
        if next != self.flux_kind_index {
            self.flux_kind_index = next;
            self.flux.reset();
            self.refresh_flux(false);
        }
    }

    fn handle_helm_key(&mut self, key: KeyEvent) {
        let Some(row) = self.helm.selected_row() else {
            return;
        };
        let table = &self.helm.table;
        let release = table.cell(row, "NAME").unwrap_or_default().to_string();
        let ns = table.cell(row, "NAMESPACE").map(String::from).or_else(|| self.namespace.clone());
        let revision: u32 = table.cell(row, "REVISION").and_then(|r| r.parse().ok()).unwrap_or(1);

        let view = |app: &mut Self, title: &str, parts: &[&str]| {
            let argv = Self::with_namespace(argv(parts), &ns);
            app.open_viewer(format!("{title} {release}"), argv, false, None);
        };
        match key.code {
            KeyCode::Enter | KeyCode::Char('s') => view(self, "status", &["helm", "status", &release]),
            KeyCode::Char('v') => view(self, "values", &["helm", "get", "values", &release]),
            KeyCode::Char('V') => view(self, "all values", &["helm", "get", "values", &release, "--all"]),
            KeyCode::Char('m') => view(self, "manifest", &["helm", "get", "manifest", &release]),
            KeyCode::Char('N') => view(self, "notes", &["helm", "get", "notes", &release]),
            KeyCode::Char('h') => view(self, "history", &["helm", "history", &release]),
            KeyCode::Char('b') => {
                self.input = revision.saturating_sub(1).max(1).to_string();
                self.mode = Mode::Prompt(Prompt::Rollback { release, namespace: ns });
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                let argv = Self::with_namespace(argv(&["helm", "uninstall", &release]), &ns);
                self.confirm(format!("Uninstall helm release {release}?"), argv);
            }
            _ => {}
        }
    }

    fn handle_flux_key(&mut self, key: KeyEvent) {
        let kind = self.flux_kind();
        match key.code {
            KeyCode::Left | KeyCode::Char('[') => return self.select_flux_kind(-1),
            KeyCode::Right | KeyCode::Char(']') => return self.select_flux_kind(1),
            KeyCode::Char('C') => {
                return self.open_viewer("flux check".into(), argv(&["flux", "check"]), false, None);
            }
            // "Flux off/on" for this kind: one `--all` command per namespace that has objects.
            KeyCode::Char('S') | KeyCode::Char('U') => {
                let verb = if key.code == KeyCode::Char('S') { "suspend" } else { "resume" };
                let namespaces: BTreeSet<String> = Self::column_values(&self.flux.table, "NAMESPACE").into_iter().collect();
                if namespaces.is_empty() {
                    return self.set_error(format!("no {} listed", kind.label));
                }
                let commands: Vec<String> = namespaces
                    .iter()
                    .map(|ns| format!("flux {verb} {} --all -n {ns}", kind.cli.join(" ")))
                    .collect();
                let message = if verb == "suspend" {
                    format!("Suspend ALL {} (Flux stops applying git changes for them)?", kind.label)
                } else {
                    format!("Resume ALL {} (Flux re-applies git state)?", kind.label)
                };
                return self.confirm(message, helpers::shell(&commands.join(" && ")));
            }
            _ => {}
        }

        let Some(row) = self.flux.selected_row() else {
            if matches!(key.code, KeyCode::Char(_) | KeyCode::Enter) {
                self.set_error(format!("no {} selected", kind.label));
            }
            return;
        };
        let table = &self.flux.table;
        let name = table.cell(row, "NAME").unwrap_or_default().to_string();
        let ns = table.cell(row, "NAMESPACE").map(String::from);
        let object = |verb: &str| {
            let mut parts = vec!["flux", verb];
            parts.extend(kind.cli);
            parts.push(&name);
            Self::with_namespace(argv(&parts), &ns)
        };

        match key.code {
            KeyCode::Enter | KeyCode::Char('d') => {
                let argv = Self::with_namespace(argv(&["kubectl", "describe", kind.crd, &name]), &ns);
                self.open_viewer(format!("describe {} {name}", kind.kind), argv, false, None);
            }
            KeyCode::Char('y') => {
                let argv = Self::with_namespace(argv(&["kubectl", "get", kind.crd, &name, "-o", "yaml"]), &ns);
                self.open_viewer(format!("yaml {} {name}", kind.kind), argv, false, None);
            }
            KeyCode::Char('r') => {
                let mut argv = object("reconcile");
                if kind.with_source {
                    argv.push("--with-source".into());
                }
                // Streams so you can watch flux's own progress output.
                self.open_viewer(format!("reconcile {} {name}", kind.kind), argv, true, None);
            }
            KeyCode::Char('s') => {
                let message = format!("Suspend {} {name}? Flux stops applying git changes to it", kind.kind);
                self.confirm(message, object("suspend"));
            }
            KeyCode::Char('u') => {
                let message = format!("Resume {} {name}? Flux re-applies git state", kind.kind);
                self.confirm(message, object("resume"));
            }
            KeyCode::Char('e') => {
                let target = format!("{}/{name}", kind.kind);
                let argv = Self::with_namespace(argv(&["flux", "events", "--for", &target]), &ns);
                self.open_viewer(format!("events {target}"), argv, false, None);
            }
            KeyCode::Char('l') => {
                let kind_flag = format!("--kind={}", kind.kind);
                let name_flag = format!("--name={name}");
                let argv = Self::with_namespace(argv(&["flux", "logs", "--follow", "--tail=200", &kind_flag, &name_flag]), &ns);
                self.open_viewer(format!("flux logs {} {name}", kind.kind), argv, true, None);
            }
            KeyCode::Char('t') if kind.tree => {
                let argv = Self::with_namespace(argv(&["flux", "tree", "kustomization", &name]), &ns);
                self.open_viewer(format!("tree {name}"), argv, false, None);
            }
            KeyCode::Char('t') => self.set_error("flux tree only works for kustomizations".into()),
            _ => {}
        }
    }

    fn handle_journal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                if let Some(command) = self.journal.selected_command() {
                    self.open_command(command);
                }
            }
            KeyCode::Char('c') => {
                self.journal.changes_only = !self.journal.changes_only;
                self.journal.clamp();
            }
            KeyCode::Char('a') => {
                self.journal.hide_auto = !self.journal.hide_auto;
                self.journal.clamp();
            }
            _ => {}
        }
    }

    fn handle_cheatsheet_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Enter
            && let Some(command) = self.cheatsheet.selected_command()
        {
            let command = match &self.namespace {
                Some(ns) => command.replace("<ns>", ns),
                None => command.to_string(),
            };
            self.open_command(command);
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        let filter = match self.tab {
            Tab::Contexts => &mut self.contexts.filter,
            Tab::Resources => &mut self.resources.filter,
            Tab::Helm => &mut self.helm.filter,
            Tab::Flux => &mut self.flux.filter,
            Tab::Journal => &mut self.journal.filter,
            Tab::Cheatsheet => &mut self.cheatsheet.filter,
        };
        match key.code {
            KeyCode::Enter => return,
            KeyCode::Esc => filter.clear(),
            KeyCode::Down | KeyCode::Up => {
                self.mode = Mode::Filter;
                return self.step(if key.code == KeyCode::Down { 1 } else { -1 });
            }
            _ => {
                helpers::edit_input(filter, key);
                self.mode = Mode::Filter;
            }
        }
        match self.tab {
            Tab::Contexts => self.contexts.clamp(),
            Tab::Resources => self.resources.clamp(),
            Tab::Helm => self.helm.clamp(),
            Tab::Flux => self.flux.clamp(),
            Tab::Journal => self.journal.clamp(),
            Tab::Cheatsheet => self.cheatsheet.clamp(),
        }
    }

    fn handle_command_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let line = std::mem::take(&mut self.input);
                self.run_command_line(&line);
                return;
            }
            KeyCode::Esc => return,
            KeyCode::Tab => self.completer.tab(&mut self.input, false),
            KeyCode::BackTab => self.completer.tab(&mut self.input, true),
            KeyCode::Up if !self.history.is_empty() => {
                let index = self.history_index.map_or(self.history.len() - 1, |i| i.saturating_sub(1));
                self.history_index = Some(index);
                self.input = self.history[index].clone();
            }
            KeyCode::Down => match self.history_index {
                Some(i) if i + 1 < self.history.len() => {
                    self.history_index = Some(i + 1);
                    self.input = self.history[i + 1].clone();
                }
                _ => {
                    self.history_index = None;
                    self.input.clear();
                }
            },
            _ => {
                helpers::edit_input(&mut self.input, key);
            }
        }
        if !matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.completer.reset();
        }
        self.mode = Mode::Command;
    }

    fn handle_prompt_key(&mut self, key: KeyEvent, prompt: Prompt) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => {}
            _ => {
                helpers::edit_input(&mut self.input, key);
                self.mode = Mode::Prompt(prompt);
                return;
            }
        }
        let value = std::mem::take(&mut self.input).trim().to_string();
        match prompt {
            Prompt::Scale { target, namespace } => {
                if value.parse::<u32>().is_err() {
                    return self.set_error(format!("'{value}' is not a replica count"));
                }
                let replicas = format!("--replicas={value}");
                let argv = Self::with_namespace(argv(&["kubectl", "scale", &target, &replicas]), &namespace);
                self.run_action(argv, &format!("Resources ▸ scale {target} to {value}"));
            }
            Prompt::Rollback { release, namespace } => {
                if value.parse::<u32>().is_err() {
                    return self.set_error(format!("'{value}' is not a revision"));
                }
                let argv = Self::with_namespace(argv(&["helm", "rollback", &release, &value]), &namespace);
                self.run_action(argv, &format!("Helm ▸ roll back {release} to revision {value}"));
            }
            Prompt::PortForward { target, namespace } => {
                let valid = value.split(':').all(|p| p.parse::<u16>().is_ok()) && !value.is_empty();
                if !valid {
                    return self.set_error(format!("'{value}' should look like LOCAL:REMOTE, e.g. 8080:80"));
                }
                let argv = Self::with_namespace(argv(&["kubectl", "port-forward", &target, &value]), &namespace);
                let local = value.split(':').next().unwrap_or(&value);
                let label = format!("localhost:{local} → {target}");
                let journal = self.journal.record(&argv, &format!("Resources ▸ port-forward {label}"), RunMode::Background, false);
                match helpers::spawn_background(&argv) {
                    Ok(child) => {
                        self.set_status(format!("port-forward started: {label}  (P to manage)"));
                        self.port_forwards.push(PortForward { label, child, journal });
                    }
                    Err(e) => {
                        let message = format!("port-forward failed: {e}");
                        self.journal.finish(journal, false, &message);
                        self.set_error(message);
                    }
                }
            }
        }
    }

    fn filtered_namespaces(&self) -> Vec<&str> {
        std::iter::once(ALL_NAMESPACES)
            .chain(self.namespaces.iter().map(String::as_str))
            .filter(|ns| helpers::matches_filter([*ns], &self.namespace_filter))
            .collect()
    }

    fn clamp_namespace_selection(&mut self) {
        let len = self.filtered_namespaces().len();
        self.namespace_state.select(helpers::clamp_selection(self.namespace_state.selected(), len));
    }

    fn handle_namespace_key(&mut self, key: KeyEvent) {
        let len = self.filtered_namespaces().len();
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Down => self.namespace_state.select(helpers::step_selection(self.namespace_state.selected(), len, 1)),
            KeyCode::Up => self.namespace_state.select(helpers::step_selection(self.namespace_state.selected(), len, -1)),
            KeyCode::Enter | KeyCode::Tab => {
                let selected = self.namespace_state.selected().and_then(|i| self.filtered_namespaces().get(i).map(|s| s.to_string()));
                let Some(selected) = selected else { return };
                self.set_namespace((selected != ALL_NAMESPACES).then_some(selected));
                match (self.namespace.clone(), key.code) {
                    (Some(ns), KeyCode::Tab) => {
                        // Persist like kubens does.
                        let flag = format!("--namespace={ns}");
                        let argv = argv(&["kubectl", "config", "set-context", "--current", &flag]);
                        self.run_action(argv, &format!("save {ns} as the context's default namespace"));
                    }
                    _ => self.set_status(format!("namespace: {}", self.namespace.as_deref().unwrap_or(ALL_NAMESPACES))),
                }
                return;
            }
            _ => {
                helpers::edit_input(&mut self.namespace_filter, key);
                self.namespace_state.select(Some(0));
                self.clamp_namespace_selection();
            }
        }
        self.mode = Mode::Namespaces;
    }

    fn handle_port_forward_key(&mut self, key: KeyEvent) {
        let len = self.port_forwards.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('P') => return,
            KeyCode::Down | KeyCode::Char('j') => self.port_forward_state.select(helpers::step_selection(self.port_forward_state.selected(), len, 1)),
            KeyCode::Up | KeyCode::Char('k') => self.port_forward_state.select(helpers::step_selection(self.port_forward_state.selected(), len, -1)),
            KeyCode::Char('x') | KeyCode::Delete => {
                if let Some(i) = self.port_forward_state.selected().filter(|&i| i < len) {
                    let mut pf = self.port_forwards.remove(i);
                    let _ = pf.child.kill();
                    let _ = pf.child.wait();
                    self.journal.finish(pf.journal, true, "stopped by you");
                    self.set_status(format!("stopped {}", pf.label));
                    self.port_forward_state.select(helpers::clamp_selection(Some(i), self.port_forwards.len()));
                }
            }
            _ => {}
        }
        self.mode = Mode::PortForwards;
    }

    fn theme(&self) -> &Theme {
        &self.themes[self.theme_index]
    }

    /// Switches to the next theme file and remembers it in config.toml.
    fn next_theme(&mut self) {
        self.theme_index = (self.theme_index + 1) % self.themes.len();
        let name = self.theme().name.clone();
        match Theme::save_choice(&name) {
            Ok(path) => self.set_status(format!("theme: {name}  (saved in {})", path.display())),
            Err(e) => self.set_error(format!("theme: {name}, but could not save the choice: {e}")),
        }
    }

    fn set_status(&mut self, message: String) {
        self.status = Some((message, false));
    }

    fn set_error(&mut self, message: String) {
        let message = message.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ⏎ ");
        self.status = Some((format!("✘ {message}"), true));
    }

    // ── drawing ───────────────────────────────────────────────────────────

    fn draw(&mut self, frame: &mut Frame) {
        let strip_height = if self.show_journal_strip && self.tab != Tab::Journal && frame.area().height > 24 {
            JOURNAL_STRIP_HEIGHT
        } else {
            0
        };
        let [header, body, strip, status, hints] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(strip_height),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let theme = self.theme().clone();
        frame.render_widget(Block::new().style(theme.base()), frame.area());
        self.draw_header(frame, header, &theme);
        match self.tab {
            Tab::Contexts => {
                let current = self.current_context.clone();
                let current_style = theme.fg(theme.success).add_modifier(Modifier::BOLD);
                self.contexts.render(frame, body, " Contexts".into(), &theme, |row| {
                    (row.get(1).is_some_and(|name| *name == current)).then_some(current_style)
                });
            }
            Tab::Resources => self.draw_resources(frame, body, &theme),
            Tab::Helm => self.helm.render(frame, body, " Helm releases".into(), &theme, |_| None),
            Tab::Flux => self.draw_flux(frame, body, &theme),
            Tab::Journal => self.journal.render(frame, body, &theme),
            Tab::Cheatsheet => self.cheatsheet.render(frame, body, &theme),
        }
        if let Some(viewer) = self.viewer.as_mut() {
            viewer.render(frame, body, &theme);
        }
        if strip_height > 0 {
            self.journal.render_strip(frame, strip, &theme);
        }
        self.draw_status(frame, status, &theme);
        self.draw_hints(frame, hints, &theme);
        self.draw_popup(frame, body, &theme);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let namespace = self.namespace.as_deref().unwrap_or("all");
        let mut info = vec![
            Span::styled(" ⎈ ", theme.fg(theme.info).bold()),
            Span::styled(self.current_context.as_str(), theme.fg(theme.success).bold()),
            Span::styled("  ns: ", theme.fg(theme.muted)),
            Span::styled(namespace, theme.fg(theme.warning).bold()),
        ];
        if self.watch {
            info.push(Span::styled(" ● watch ", theme.fg(theme.success)));
        }
        if !self.port_forwards.is_empty() {
            info.push(Span::styled(format!(" ⇄ {} ", self.port_forwards.len()), theme.fg(theme.secondary)));
        }
        let info = Line::from(info);
        let [tabs_area, info_area] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(info.width() as u16 + 1)]).areas(area);

        let tabs = Tabs::new(Tab::ALL.iter().map(|t| t.title()))
            .select(self.tab.index())
            .style(theme.fg(theme.muted))
            .highlight_style(theme.badge(theme.accent).bold())
            .divider(Span::styled("│", theme.fg(theme.border)));
        frame.render_widget(tabs, tabs_area);
        frame.render_widget(Paragraph::new(info).right_aligned(), info_area);
    }

    /// Left-hand list of kinds used by the Resources and Flux tabs.
    fn draw_kind_list<'a>(
        frame: &mut Frame,
        area: Rect,
        items: impl IntoIterator<Item = ListItem<'a>>,
        selected: usize,
        theme: &Theme,
    ) {
        let list = List::new(items)
            .block(theme.block().title(" Kinds ").title_bottom(Line::from(" ←/→ [ ] ").style(theme.fg(theme.muted))))
            .highlight_style(theme.badge(theme.accent).bold())
            .highlight_symbol("▶ ");
        let mut state = ListState::default().with_selected(Some(selected));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn draw_resources(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let [kinds_area, table_area] = Layout::horizontal([Constraint::Length(28), Constraint::Fill(1)]).areas(area);
        let items = KINDS.iter().map(|k| {
            let item = ListItem::new(k.name);
            if k.namespaced { item } else { item.style(theme.fg(theme.muted)) }
        });
        Self::draw_kind_list(frame, kinds_area, items, self.kind_index, theme);

        let title = format!(" {}", self.kind().name);
        self.resources.render(frame, table_area, title, theme, |_| None);
    }

    fn draw_flux(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let [kinds_area, table_area] = Layout::horizontal([Constraint::Length(22), Constraint::Fill(1)]).areas(area);
        Self::draw_kind_list(frame, kinds_area, FLUX_KINDS.iter().map(|k| ListItem::new(k.label)), self.flux_kind_index, theme);

        let title = format!(" Flux {} (all namespaces)", self.flux_kind().label);
        let suspended = self.flux.table.column("SUSPENDED");
        let suspended_style = theme.fg(theme.warning).add_modifier(Modifier::DIM);
        self.flux.render(frame, table_area, title, theme, |row| {
            suspended.and_then(|i| row.get(i)).is_some_and(|s| s == "True").then_some(suspended_style)
        });
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let line = match &self.mode {
            Mode::Command => Line::from(vec![
                Span::styled(":", theme.fg(theme.accent).bold()),
                Span::raw(self.input.as_str()),
                Span::styled("█", theme.fg(theme.accent)),
            ]),
            Mode::Filter => {
                let filter = match self.tab {
                    Tab::Contexts => &self.contexts.filter,
                    Tab::Resources => &self.resources.filter,
                    Tab::Helm => &self.helm.filter,
                    Tab::Flux => &self.flux.filter,
                    Tab::Journal => &self.journal.filter,
                    Tab::Cheatsheet => &self.cheatsheet.filter,
                };
                Line::from(vec![
                    Span::styled("/", theme.fg(theme.warning).bold()),
                    Span::raw(filter.as_str()),
                    Span::styled("█", theme.fg(theme.warning)),
                ])
            }
            _ => match &self.status {
                Some((message, true)) => Line::styled(message.as_str(), theme.fg(theme.danger)),
                Some((message, false)) => Line::styled(message.as_str(), theme.fg(theme.success)),
                None => Line::from(""),
            },
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    fn draw_hints(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let resource_hints;
        let hints: &[(&str, &str)] = match (&self.mode, self.viewer.is_some(), self.tab) {
            (Mode::Command, ..) => &[("Tab", "complete"), ("Enter", "run"), ("!cmd", "interactive"), ("watch cmd", "repeat"), ("↑↓", "history"), ("Esc", "cancel")],
            (Mode::Filter, ..) => &[("type", "filter"), ("Enter", "keep"), ("Esc", "clear")],
            (Mode::Confirm { .. }, ..) => &[("y", "run it"), ("e", "edit in prompt first"), ("n", "cancel")],
            (_, true, _) => &[("q", "close"), ("w", "watch"), ("r", "rerun"), ("f", "follow")],
            (_, _, Tab::Contexts) => &[("Enter", "use context"), ("i", "cluster-info"), ("v", "version")],
            (_, _, Tab::Resources) => {
                // Only advertise what works for the selected kind.
                let kind = self.kind();
                let optional = [
                    (kind.logs, ("l", "logs")),
                    (kind.name == "pods", ("L", "prev logs")),
                    (kind.exec, ("e", "exec")),
                    (kind.forward, ("f", "fwd")),
                    (kind.scale, ("s", "scale")),
                    (kind.restart, ("r", "restart")),
                    (kind.restart, ("o", "rollout")),
                ];
                resource_hints = [("←→", "kind"), ("d", "describe"), ("y", "yaml"), ("v", "events")]
                    .into_iter()
                    .chain(optional.into_iter().filter(|(on, _)| *on).map(|(_, hint)| hint))
                    .chain([("E", "edit"), ("x", "delete")])
                    .collect::<Vec<_>>();
                &resource_hints
            }
            (_, _, Tab::Helm) => &[
                ("s", "status"), ("v/V", "values"), ("m", "manifest"), ("N", "notes"), ("h", "history"), ("b", "rollback"), ("x", "uninstall"),
            ],
            (_, _, Tab::Flux) => &[
                ("←→", "kind"), ("r", "reconcile"), ("s/u", "suspend/resume"), ("S/U", "ALL off/on"), ("d", "describe"),
                ("y", "yaml"), ("e", "events"), ("l", "logs"), ("t", "tree"), ("C", "check"),
            ],
            (_, _, Tab::Journal) => &[("Enter", "load into prompt"), ("c", "changes only"), ("a", "hide auto")],
            (_, _, Tab::Cheatsheet) => &[("/", "search"), ("Enter", "load into prompt")],
        };
        let mut spans = Vec::new();
        for (key, what) in hints.iter().chain(&[("Space", "views"), ("J", "journal"), ("T", "theme"), (":", "cmd"), ("?", "help"), ("q", "quit")]) {
            spans.push(Span::styled(format!(" {key} "), theme.chip()));
            spans.push(Span::styled(format!(" {what} "), theme.fg(theme.text)));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_popup(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        match &self.mode {
            Mode::Confirm { message, argv } => {
                let command = helpers::shell_join(argv);
                let mut text = vec![Line::from(message.as_str()).bold(), Line::from(""), Line::styled("This runs:", theme.fg(theme.muted))];
                // `a && b` chains read better one command per line.
                for part in command.split(" && ") {
                    text.push(Line::styled(format!("  $ {part}"), theme.fg(theme.warning)));
                }
                text.push(Line::from(""));
                text.push(Line::from(vec![
                    Span::styled(" y ", theme.badge(theme.danger)),
                    Span::raw(" run it   "),
                    Span::styled(" e ", theme.badge(theme.accent)),
                    Span::raw(" edit in prompt first   "),
                    Span::styled(" n ", theme.chip()),
                    Span::raw(" cancel"),
                ]));
                let height = text.len() as u16 + 2;
                Self::popup(frame, area, " Confirm ", theme, theme.danger, 90, height, Paragraph::new(text).wrap(Wrap { trim: false }));
            }
            Mode::Prompt(prompt) => {
                let label = match prompt {
                    Prompt::Scale { target, .. } => format!("Replicas for {target}:"),
                    Prompt::PortForward { target, .. } => format!("Ports for {target} (LOCAL:REMOTE):"),
                    Prompt::Rollback { release, .. } => format!("Roll back {release} to revision:"),
                };
                let text = vec![
                    Line::from(label),
                    Line::from(vec![Span::raw(format!("> {}", self.input)), Span::styled("█", theme.fg(theme.accent))]),
                    Line::styled(" Enter confirm · Esc cancel", theme.fg(theme.muted)),
                ];
                Self::popup(frame, area, " Input ", theme, theme.accent, 60, 5, Paragraph::new(text));
            }
            Mode::Namespaces => {
                let namespaces: Vec<String> = self.filtered_namespaces().into_iter().map(String::from).collect();
                let popup = area.centered(Constraint::Length(50.min(area.width)), Constraint::Percentage(70));
                let items: Vec<ListItem> = namespaces
                    .iter()
                    .map(|ns| {
                        let current = self.namespace.as_deref().unwrap_or(ALL_NAMESPACES) == ns.as_str();
                        let item = ListItem::new(ns.clone());
                        if current { item.style(theme.fg(theme.success).bold()) } else { item }
                    })
                    .collect();
                let title = format!(" Namespace: {}█ ", self.namespace_filter);
                let list = List::new(items)
                    .block(
                        Block::bordered()
                            .style(theme.popup())
                            .border_style(theme.fg(theme.warning))
                            .title(title)
                            .title_bottom(Line::styled(" Enter select · Tab select + save to context · Esc ", theme.fg(theme.muted))),
                    )
                    .highlight_style(theme.selected())
                    .highlight_symbol("▶ ");
                frame.render_widget(Clear, popup);
                frame.render_stateful_widget(list, popup, &mut self.namespace_state);
            }
            Mode::PortForwards => {
                let popup = area.centered(Constraint::Length(70.min(area.width)), Constraint::Length(12));
                let items: Vec<ListItem> = if self.port_forwards.is_empty() {
                    vec![ListItem::new("No port-forwards running. Select a pod/service and press f.").style(theme.fg(theme.muted))]
                } else {
                    self.port_forwards.iter().map(|pf| ListItem::new(pf.label.as_str())).collect()
                };
                let list = List::new(items)
                    .block(
                        Block::bordered()
                            .style(theme.popup())
                            .border_style(theme.fg(theme.secondary))
                            .title(" Port-forwards ")
                            .title_bottom(Line::styled(" x stop · Esc close ", theme.fg(theme.muted))),
                    )
                    .highlight_style(theme.selected())
                    .highlight_symbol("▶ ");
                frame.render_widget(Clear, popup);
                frame.render_stateful_widget(list, popup, &mut self.port_forward_state);
            }
            Mode::Help => {
                let mut lines: Vec<Line> = HELP.lines().map(Line::from).collect();
                lines.push(Line::from("ALIASES (command prompt)"));
                for pair in helpers::ALIASES.chunks(2) {
                    let text: String = pair.iter().map(|(a, e)| format!("  {a:<5} {e:<37}")).collect();
                    lines.push(Line::styled(text, theme.fg(theme.muted)));
                }
                let height = lines.len() as u16 + 2;
                Self::popup(frame, area, " Help ", theme, theme.accent, 96, height, Paragraph::new(lines));
            }
            Mode::Leader => {
                let lines: Vec<Line> = LEADER_KEYS
                    .chunks(2)
                    .map(|pair| {
                        let mut spans = Vec::new();
                        for (key, what) in pair {
                            spans.push(Span::styled(format!(" {key} "), theme.badge(theme.accent)));
                            spans.push(Span::raw(format!(" {what:<26}")));
                        }
                        Line::from(spans)
                    })
                    .collect();
                let height = lines.len() as u16 + 2;
                Self::popup(frame, area, " Space … ", theme, theme.accent, 66, height, Paragraph::new(lines));
            }
            Mode::Command => self.draw_suggestions(frame, area, theme),
            Mode::Normal | Mode::Filter => {}
        }
    }

    /// Completion candidates listed just above the prompt line.
    fn draw_suggestions(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let (candidates, selected) = self.completer.suggestions(&self.input);
        if candidates.is_empty() {
            return;
        }
        const VISIBLE: usize = 10;
        // Keep the selected candidate in view while cycling.
        let offset = selected.map_or(0, |s| s.saturating_sub(VISIBLE - 1));
        let width = candidates.iter().map(|c| c.chars().count()).max().unwrap_or(0).clamp(20, 70) as u16 + 4;
        let height = candidates.len().min(VISIBLE) as u16 + 2;
        let popup = Rect {
            x: area.x + 1,
            y: area.bottom().saturating_sub(height),
            width: width.min(area.width.saturating_sub(1)),
            height: height.min(area.height),
        };
        let lines: Vec<Line> = candidates
            .iter()
            .enumerate()
            .skip(offset)
            .take(VISIBLE)
            .map(|(i, c)| {
                if Some(i) == selected {
                    Line::styled(format!(" {c}"), theme.selected())
                } else {
                    Line::from(format!(" {c}"))
                }
            })
            .collect();
        let title = format!(" Tab ({}) ", candidates.len());
        let block = Block::bordered().style(theme.popup()).title(title).border_style(theme.fg(theme.muted));
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(lines).block(block), popup);
    }

    #[allow(clippy::too_many_arguments)]
    fn popup(frame: &mut Frame, area: Rect, title: &str, theme: &Theme, color: Color, width: u16, height: u16, content: Paragraph) {
        let popup = area.centered(Constraint::Length(width.min(area.width)), Constraint::Length(height.min(area.height)));
        let block = Block::bordered().style(theme.popup()).title(title).border_style(theme.fg(color));
        frame.render_widget(Clear, popup);
        frame.render_widget(content.block(block), popup);
    }
}

const HELP: &str = "\
GLOBAL
  Space          leader: open a view     (Space s = services, Space a = all namespaces, ...)
  1-6 / Tab      switch tab              n        pick namespace (Tab saves it)
  /              filter current list     w        auto-refresh every 2s
  :              run any command         R / F5   refresh
  J              toggle command strip    P        manage port-forwards      q  quit
  T              next color theme (files in ~/.config/kubewatcher/themes/, choice in config.toml)
EVERY CHANGE ASKS FIRST
  The confirm dialog shows the exact command:  y run it · e edit it in the prompt · n cancel
COMMAND PROMPT (:)
  Tab / Shift-Tab           complete verbs, kinds, live resource names, namespaces, releases
  kubectl get pods -w       streams live      watch -n 5 <cmd>   re-runs every 5s
  !<cmd>                    runs on the real terminal (exec -it, edit, k9s...)
RESOURCES
  ←/→ [ ]  kind     d describe   y yaml     v events     l logs -f   L previous logs
  e exec shell      E edit       f port-forward          s scale     r restart
  o rollout status  x delete
HELM
  s status  v values  V all values  m manifest  N notes  h history  b rollback  x uninstall
FLUX
  r reconcile now   s/u suspend/resume one   S/U suspend/resume ALL of this kind (Flux off/on)
  d describe  y yaml  e events  l controller logs  t tree (kustomizations)  C flux check
JOURNAL (5)
  every command kubewatcher runs, with time, duration, result and what triggered it
  saved to ~/.local/state/kubewatcher/journal/YYYY-MM-DD.log (one file per day)
  Enter loads a command into the prompt · c changes only · a hide automatic refreshes
VIEWER
  j/k space/u g/G scroll · h/l sideways · w toggle watch · r rerun · f follow · q close";
