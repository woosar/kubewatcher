use std::collections::BTreeSet;
use std::io::{self, Read};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use ratatui::backend::Backend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, TableState, Tabs, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::Value;

use crate::data_structures::{
    COMPLETION_KINDS, Cheatsheet, Completer, ContextForm, FormAction, Host, Journal, K3sForm, KubeTable, Machine, Msg, RunMode, Target,
    Theme, Tool, ToolKind, Viewer, ViewerAction,
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
    Tooling,
    Journal,
    Cheatsheet,
}

impl Tab {
    const ALL: [Tab; 7] = [Tab::Contexts, Tab::Resources, Tab::Helm, Tab::Flux, Tab::Tooling, Tab::Journal, Tab::Cheatsheet];

    fn title(self) -> &'static str {
        match self {
            Tab::Contexts => "1 Contexts",
            Tab::Resources => "2 Resources",
            Tab::Helm => "3 Helm",
            Tab::Flux => "4 Flux",
            Tab::Tooling => "5 Tooling",
            Tab::Journal => "6 Journal",
            Tab::Cheatsheet => "7 Cheatsheet",
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
    AddContext(ContextForm),
    ImportK3s(K3sForm),
    /// Like Confirm, for changes kubewatcher makes itself (rc file, splitting kubeconfigs).
    ConfirmPlan { title: String, lines: Vec<String>, plan: Plan },
    /// Tools are missing: only the Tooling tab works until they are installed.
    Blocked { missing: Vec<String> },
    Namespaces,
    PortForwards,
    Help,
    /// Waiting for the key after the leader (Space).
    Leader,
}

/// What a ConfirmPlan does once confirmed.
enum Plan {
    /// Writes the shell wiring, then runs the install commands.
    Tooling { argv: Option<Vec<String>>, shell: bool },
    /// Splits these kubeconfig files into one file per context.
    Restructure { sources: Vec<PathBuf>, stamp: String },
}

/// A kubeconfig file kubectl reads and the contexts it defines.
struct KubeconfigFile {
    path: PathBuf,
    contexts: Vec<String>,
    current: String,
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
    ('o', "tooling"),
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
    /// Set when the command prints a k3s.yaml to build this context from.
    k3s: Option<K3sForm>,
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
    host: Box<dyn Host>,
    /// `~/.config/kubewatcher`: themes and config.toml.
    config_dir: Option<PathBuf>,
    /// Home, search path and shell; None without a home directory.
    machine: Option<Machine>,
    /// Tools are missing: everything but the Tooling tab is locked.
    locked: bool,
    tools: Vec<Tool>,
    tooling: TablePane,
    /// The files kubectl reads, in order, and their contexts.
    kubeconfig: Vec<KubeconfigFile>,
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
    /// The real app: system processes and terminal, XDG config and state directories.
    #[cfg(not(test))]
    pub fn new() -> Self {
        let config_dir = helpers::xdg_dir("XDG_CONFIG_HOME", &[".config"]).map(|d| d.join("kubewatcher"));
        let journal_dir = helpers::xdg_dir("XDG_STATE_HOME", &[".local", "state"]).map(|d| d.join("kubewatcher").join("journal"));
        let machine = Machine::from_env();
        if let Some(machine) = &machine {
            // Tools installed into ~/.local/bin work right away, whatever the shell's PATH is.
            helpers::set_child_env("PATH", machine.search_path.clone());
        }
        Self::with_host(Box::new(crate::data_structures::SystemHost), config_dir, journal_dir, machine)
    }

    fn with_host(host: Box<dyn Host>, config_dir: Option<PathBuf>, journal_dir: Option<PathBuf>, machine: Option<Machine>) -> Self {
        let (tx, rx) = mpsc::channel();
        let (themes, theme_index, theme_problems) = Theme::load_all(config_dir.as_deref());
        let status = theme_problems.first().map(|p| (format!("✘ theme: {p}"), true));
        let tools = machine.as_ref().map(Tool::all).unwrap_or_default();
        let missing: Vec<String> = tools.iter().filter(|t| !t.present).map(|t| t.name().to_string()).collect();
        let locked = !missing.is_empty();
        Self {
            host,
            config_dir,
            machine,
            locked,
            tools,
            tooling: TablePane::default(),
            kubeconfig: Vec::new(),
            tx,
            rx,
            running: true,
            tab: if locked { Tab::Tooling } else { Tab::Resources },
            mode: if locked { Mode::Blocked { missing } } else { Mode::Normal },
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
            journal: Journal::open(journal_dir),
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

    pub fn run<B: Backend>(mut self, terminal: &mut Terminal<B>) -> io::Result<()>
    where
        B::Error: Send + Sync + 'static,
    {
        self.refresh_tooling(true);
        // Resources, helm and flux are loaded once the context's namespace is known.
        if !self.locked {
            self.refresh_contexts(false);
            self.refresh_namespaces();
        }
        while self.running {
            terminal.draw(|frame| self.draw(frame)).map_err(io::Error::other)?;
            if let Some(Event::Key(key)) = self.host.next_event(Duration::from_millis(100))?
                && key.kind == KeyEventKind::Press
            {
                self.handle_key(key);
            }
            while let Ok(msg) = self.rx.try_recv() {
                self.handle_msg(msg);
            }
            self.tick();
            if let Some(mut job) = self.pending_interactive.take() {
                if let Some(form) = job.k3s.take() {
                    let output = self.host.interactive_capture(&job.argv, "apiVersion:")?;
                    terminal.clear().map_err(io::Error::other)?;
                    self.finish_k3s_import(job.journal, &form, output);
                    continue;
                }
                let outcome = self.host.interactive(&job.argv, job.pause)?;
                terminal.clear().map_err(io::Error::other)?;
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
            Target::Tooling { row, latest } => self.on_tooling(row, latest, result),
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
        let mut parsed = result.map(|text| KubeTable::parse(&text));
        if let Ok(table) = &mut parsed {
            self.load_kubeconfig_files();
            self.add_file_column(table);
        }
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

    fn on_tooling(&mut self, row: usize, latest: bool, result: Result<String, String>) {
        let Some(tool) = self.tools.get_mut(row) else { return };
        match (latest, result) {
            (true, Ok(text)) => tool.latest = Tool::parse_latest(&text),
            (false, Ok(text)) => tool.installed = helpers::first_version(&text),
            (_, Err(e)) => tool.problem = Some(e),
        }
        self.update_tooling_table();
    }

    /// Points KUBECONFIG at ~/.kube/config plus every file in config.d, like the shell's
    /// `kubeconfig_rebuild`, so added and split contexts show up right away. Files from the
    /// KUBECONFIG kubewatcher was started with stay in, or their contexts would vanish.
    fn rebuild_kubeconfig(&mut self) -> Vec<PathBuf> {
        let Some(machine) = &self.machine else { return Vec::new() };
        let inherited = std::env::var_os("KUBECONFIG");
        match helpers::rebuild_kubeconfig(&machine.home, inherited.as_deref()) {
            Some(value) => {
                let files = std::env::split_paths(&value).collect();
                helpers::set_child_env("KUBECONFIG", value);
                files
            }
            None => helpers::kubeconfig_files(&machine.home),
        }
    }

    /// Reads which file defines which context (`kubectl config view` per file; secrets stay redacted).
    fn load_kubeconfig_files(&mut self) {
        self.kubeconfig.clear();
        for path in self.rebuild_kubeconfig().into_iter().filter(|p| p.is_file()) {
            let argv = argv(&["kubectl", "config", "view", &format!("--kubeconfig={}", path.display()), "-o", "json"]);
            let journal = self.journal.record(&argv, "Contexts ▸ which file holds which context", RunMode::Capture, true);
            let result = self.host.run(&argv);
            let json = result.as_ref().ok().and_then(|text| serde_json::from_str::<Value>(text).ok());
            let names = |json: &Value| -> Vec<String> {
                let contexts = json["contexts"].as_array().cloned().unwrap_or_default();
                contexts.iter().filter_map(|c| c["name"].as_str().map(String::from)).collect()
            };
            let contexts = json.as_ref().map(names).unwrap_or_default();
            let current = json.as_ref().and_then(|j| j["current-context"].as_str()).unwrap_or_default().to_string();
            match &result {
                Ok(_) => self.journal.finish(journal, true, &format!("{} context(s)", contexts.len())),
                Err(e) => self.journal.finish(journal, false, e),
            }
            self.kubeconfig.push(KubeconfigFile { path, contexts, current });
        }
    }

    /// The file kubectl takes a context from: the first one that defines it.
    fn file_of(&self, context: &str) -> Option<&KubeconfigFile> {
        self.kubeconfig.iter().find(|f| f.contexts.iter().any(|c| c == context))
    }

    fn add_file_column(&self, table: &mut KubeTable) {
        let Some(machine) = &self.machine else { return };
        if table.headers.is_empty() {
            return;
        }
        let files: Vec<String> = table
            .rows
            .iter()
            .map(|row| {
                let file = table.cell(row, "NAME").and_then(|name| self.file_of(name));
                file.map(|f| machine.display(&f.path)).unwrap_or_default()
            })
            .collect();
        table.headers.push("FILE".into());
        for (row, file) in table.rows.iter_mut().zip(files) {
            row.push(file);
        }
    }

    /// Files holding more than one context, which `S` splits; only ~/.kube/config and config.d.
    fn files_to_split(&self) -> Vec<&KubeconfigFile> {
        let Some(machine) = &self.machine else { return Vec::new() };
        self.kubeconfig
            .iter()
            .filter(|f| f.contexts.len() > 1 && (f.path == machine.kube_config() || f.path.parent() == Some(machine.config_d().as_path())))
            .collect()
    }

    fn update_tooling_table(&mut self) {
        let Some(machine) = &self.machine else { return };
        let headers = ["TOOL", "INSTALLED", "LATEST", "LOCATION", "STATE"].map(String::from).to_vec();
        let rows = self.tools.iter().map(|t| t.row(machine)).collect();
        self.tooling.set_result(Ok(KubeTable { headers, rows }));
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
        self.host.capture(argv, target, journal, self.tx.clone());
    }

    /// Hands the real terminal to a command after this frame.
    fn queue_interactive(&mut self, argv: Vec<String>, pause: bool, origin: &str) {
        let journal = self.journal.record(&argv, origin, RunMode::Interactive, false);
        self.pending_interactive = Some(Interactive { argv, pause, journal, k3s: None });
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
        self.rebuild_kubeconfig();
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

    /// Re-checks what is installed and reads the versions in the background. The newest
    /// releases are looked up once, and again on `r`.
    fn refresh_tooling(&mut self, auto: bool) {
        let Some(machine) = self.machine.clone() else { return };
        for row in 0..self.tools.len() {
            self.tools[row].check(&machine);
            if let Some(argv) = self.tools[row].version_argv() {
                self.capture(argv, Target::Tooling { row, latest: false }, "Tooling ▸ installed version", true);
            }
            if (!auto || self.tools[row].latest.is_none())
                && let Some(argv) = self.tools[row].latest_argv()
            {
                self.tools[row].problem = None;
                self.capture(argv, Target::Tooling { row, latest: true }, "Tooling ▸ latest release", true);
            }
        }
        self.tooling.command = "<tool> --version · curl …/releases/latest".into();
        self.update_tooling_table();
        if self.locked && self.tools.iter().all(|t| t.present) {
            self.locked = false;
            self.set_status("✔ everything is installed; kubewatcher is ready".into());
            self.refresh_contexts(false);
            self.refresh_namespaces();
        }
    }

    fn refresh_current(&mut self, auto: bool) {
        match self.tab {
            Tab::Contexts => self.refresh_contexts(auto),
            Tab::Tooling => self.refresh_tooling(auto),
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
            match self.host.stream(&viewer.argv, generation, &self.tx) {
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
            self.host.capture(viewer.argv.clone(), Target::Viewer(generation), viewer.journal, self.tx.clone());
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
            self.host.capture(viewer.argv.clone(), Target::Viewer(viewer.generation), viewer.journal, self.tx.clone());
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
            Mode::AddContext(form) => self.handle_add_context_key(key, form),
            Mode::ImportK3s(form) => self.handle_import_k3s_key(key, form),
            Mode::ConfirmPlan { title, lines, plan } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.execute_plan(plan),
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => self.set_status("cancelled".into()),
                _ => self.mode = Mode::ConfirmPlan { title, lines, plan },
            },
            Mode::Blocked { missing } => match key.code {
                KeyCode::Char('q') => self.running = false,
                KeyCode::Enter | KeyCode::Esc => self.tab = Tab::Tooling,
                _ => self.mode = Mode::Blocked { missing },
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
            'o' => self.tab = Tab::Tooling,
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

        if self.locked && !self.allowed_while_locked(key) {
            self.tab = Tab::Tooling;
            return self.set_error("tools are missing: install them here first (Enter/u one, U all, r re-check)".into());
        }

        match key.code {
            KeyCode::Char('q') => self.running = false,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char(' ') => self.mode = Mode::Leader,
            KeyCode::Char(':') => self.open_command(String::new()),
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char(c @ '1'..='7') => self.tab = Tab::ALL[c as usize - '1' as usize],
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
                Tab::Tooling => self.handle_tooling_key(key),
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
            Tab::Tooling => self.tooling.step(delta),
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
            KeyCode::Char('a') if self.machine.is_some() => self.mode = Mode::AddContext(ContextForm::default()),
            KeyCode::Char('K') if self.machine.is_some() => self.mode = Mode::ImportK3s(K3sForm::default()),
            KeyCode::Char('x') => self.remove_context(),
            KeyCode::Char('S') => self.plan_restructure(),
            _ => {}
        }
    }

    fn selected_context(&self) -> Option<String> {
        self.contexts.selected_row().and_then(|r| self.contexts.table.cell(r, "NAME")).map(String::from)
    }

    /// Deletes the file of a context that has a file of its own in config.d.
    fn remove_context(&mut self) {
        let (Some(name), Some(machine)) = (self.selected_context(), self.machine.clone()) else { return };
        let Some(file) = self.file_of(&name) else {
            return self.set_error(format!("no kubeconfig file defines {name}"));
        };
        let shown = machine.display(&file.path);
        if file.contexts.len() > 1 {
            return self.set_error(format!("{name} shares {shown} with other contexts; S gives each its own file first"));
        }
        if file.path.parent() != Some(machine.config_d().as_path()) {
            return self.set_error(format!("{name} lives in {shown}, which stays the write target; use kubectl config delete-context"));
        }
        let argv = argv(&["rm", &file.path.display().to_string()]);
        self.confirm(format!("Remove context {name} by deleting {shown}?"), argv);
    }

    fn handle_add_context_key(&mut self, key: KeyEvent, mut form: ContextForm) {
        match form.handle_key(key) {
            FormAction::Cancel => return self.set_status("cancelled".into()),
            FormAction::None => {}
            FormAction::Submit => {
                let Some(machine) = self.machine.clone() else { return };
                let name = form.name().to_string();
                // On a name clash the first kubeconfig file wins silently, so names stay unique.
                if Self::column_values(&self.contexts.table, "NAME").contains(&name) {
                    self.set_error(format!("context {name} already exists; pick another name"));
                } else {
                    let file = helpers::context_file_path(&machine.config_d(), &name);
                    match form.commands(&file) {
                        Ok(commands) => return self.save_context(&name, &commands),
                        Err(e) => self.set_error(e),
                    }
                }
            }
        }
        self.mode = Mode::AddContext(form);
    }

    fn handle_import_k3s_key(&mut self, key: KeyEvent, mut form: K3sForm) {
        match form.handle_key(key) {
            FormAction::Cancel => return self.set_status("cancelled".into()),
            FormAction::None => {}
            FormAction::Submit => {
                let name = form.name().to_string();
                // Checked before ssh, so no passwords are typed for nothing.
                if Self::column_values(&self.contexts.table, "NAME").contains(&name) {
                    self.set_error(format!("context {name} already exists; pick another name"));
                } else {
                    match form.fetch() {
                        Ok(argv) => {
                            let journal = self.journal.record(&argv, &format!("Contexts ▸ import {name} from k3s"), RunMode::Interactive, false);
                            self.pending_interactive = Some(Interactive { argv, pause: false, journal, k3s: Some(form) });
                            return;
                        }
                        Err(e) => self.set_error(e),
                    }
                }
            }
        }
        self.mode = Mode::ImportK3s(form);
    }

    /// Writes the context from what `ssh … sudo cat k3s.yaml` printed.
    fn finish_k3s_import(&mut self, journal: u64, form: &K3sForm, output: Result<String, String>) {
        let Some(machine) = self.machine.clone() else { return };
        let commands = output.and_then(|text| form.commands(&helpers::context_file_path(&machine.config_d(), form.name()), &text));
        match commands {
            Ok(commands) => {
                self.journal.finish(journal, true, "read k3s.yaml (client certificate and key kept, not logged)");
                self.save_context(form.name(), &commands);
            }
            Err(e) => {
                self.journal.finish(journal, false, &e);
                self.set_error(e);
            }
        }
    }

    /// Runs the `kubectl config` writes; the journal and status bar only ever see masked
    /// secrets: every value set for a `…-data` property (client certificate and key).
    fn save_context(&mut self, name: &str, commands: &[Vec<String>]) {
        let join = |mask: bool| {
            let mask_secret = |(i, arg): (usize, &String), argv: &[String]| {
                if mask && i > 0 && argv[i - 1].ends_with("-data") { "••••".to_string() } else { arg.clone() }
            };
            let render = |argv: &Vec<String>| helpers::shell_join(&argv.iter().enumerate().map(|a| mask_secret(a, argv)).collect::<Vec<_>>());
            commands.iter().map(render).collect::<Vec<_>>().join(" && ")
        };
        let shown = helpers::shell(&join(true));
        self.set_status(format!("$ {}", join(true)));
        let journal = self.journal.record(&shown, &format!("Contexts ▸ add {name}"), RunMode::Capture, false);
        self.host.capture(helpers::shell(&join(false)), Target::Action, journal, self.tx.clone());
    }

    /// Asks before giving every context in a shared file a file of its own.
    fn plan_restructure(&mut self) {
        let Some(machine) = self.machine.clone() else { return };
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let mut lines = Vec::new();
        let mut sources = Vec::new();
        for file in self.files_to_split() {
            let shown = machine.display(&file.path);
            let targets: Vec<String> = file.contexts.iter().map(|c| format!("config.d/{}.yaml", helpers::context_file_name(c))).collect();
            lines.push(format!("{shown} → {}", targets.join(", ")));
            lines.push(format!("  backup first: {shown}.bak-{stamp}"));
            lines.push(if file.path == machine.kube_config() {
                "  then it keeps only current-context (kubectx/kubens write there)".to_string()
            } else {
                "  then it is deleted".to_string()
            });
            sources.push(file.path.clone());
        }
        if sources.is_empty() {
            return self.set_status("every context already has a file of its own".into());
        }
        let title = "Give every context its own file in ~/.kube/config.d?".to_string();
        self.mode = Mode::ConfirmPlan { title, lines, plan: Plan::Restructure { sources, stamp } };
    }

    fn execute_plan(&mut self, plan: Plan) {
        match plan {
            Plan::Tooling { argv, shell } => {
                if shell {
                    self.write_shell_wiring();
                }
                match argv {
                    Some(argv) => self.run_action(argv, "Tooling ▸ install"),
                    None => self.refresh_tooling(true),
                }
            }
            Plan::Restructure { sources, stamp } => {
                let mut written = 0;
                for source in sources {
                    match self.split_file(&source, &stamp) {
                        Ok(count) => written += count,
                        Err(e) => {
                            self.set_error(format!("split {} stopped: {e} (backup kept)", source.display()));
                            return self.refresh_contexts(false);
                        }
                    }
                }
                self.set_status(format!("✔ {written} context(s) now in a file of their own; backups end in .bak-{stamp}"));
                self.refresh_contexts(false);
            }
        }
    }

    /// Records a step kubewatcher does itself in the journal, as the equivalent command.
    fn journal_step(&mut self, argv: Vec<String>, origin: &str, outcome: Result<String, String>) -> Result<String, String> {
        let journal = self.journal.record(&argv, origin, RunMode::Capture, false);
        match &outcome {
            Ok(message) => self.journal.finish(journal, true, message),
            Err(e) => self.journal.finish(journal, false, e),
        }
        outcome
    }

    /// Backs up `source`, writes each of its contexts into config.d and empties it.
    /// File contents hold credentials and never reach the journal.
    fn split_file(&mut self, source: &Path, stamp: &str) -> Result<usize, String> {
        let Some(machine) = self.machine.clone() else { return Ok(0) };
        let Some((contexts, current)) = self.kubeconfig.iter().find(|f| f.path == source).map(|f| (f.contexts.clone(), f.current.clone())) else {
            return Ok(0);
        };
        let origin = "Contexts ▸ one file per context";
        let display = |path: &Path| path.display().to_string();
        let backup = PathBuf::from(format!("{}.bak-{stamp}", source.display()));
        let copied = fs::read_to_string(source).and_then(|text| helpers::write_private(&backup, &text));
        let outcome = copied.map(|()| "backup written".to_string()).map_err(|e| e.to_string());
        self.journal_step(argv(&["cp", &display(source), &display(&backup)]), origin, outcome)?;

        fs::create_dir_all(machine.config_d()).map_err(|e| e.to_string())?;
        for context in &contexts {
            let view = argv(&["kubectl", "config", "view", &format!("--kubeconfig={}", source.display()), &format!("--context={context}"), "--minify", "--flatten", "--raw"]);
            let target = helpers::context_file_path(&machine.config_d(), context);
            let journal = self.journal.record(&view, origin, RunMode::Capture, false);
            let written = self.host.run(&view).and_then(|text| {
                let text: String = text.lines().filter(|l| !l.starts_with("current-context:")).map(|l| format!("{l}\n")).collect();
                helpers::write_private(&target, &text).map_err(|e| e.to_string())
            });
            match written {
                Ok(()) => self.journal.finish(journal, true, &format!("written to {} (0600, contents not logged)", machine.display(&target))),
                Err(e) => {
                    self.journal.finish(journal, false, &e);
                    return Err(e);
                }
            }
        }

        if source == machine.kube_config() {
            let current = serde_json::to_string(&current).unwrap_or_default();
            let text = format!("apiVersion: v1\nkind: Config\nclusters: []\ncontexts: []\nusers: []\ncurrent-context: {current}\npreferences: {{}}\n");
            let outcome = helpers::write_private(source, &text).map(|()| "kept only current-context".to_string()).map_err(|e| e.to_string());
            let shown = helpers::shell(&format!("# kubewatcher: keep only current-context in {}", display(source)));
            self.journal_step(shown, origin, outcome)?;
        } else {
            let outcome = fs::remove_file(source).map(|()| "deleted".to_string()).map_err(|e| e.to_string());
            self.journal_step(argv(&["rm", &display(source)]), origin, outcome)?;
        }
        Ok(contexts.len())
    }

    fn handle_tooling_key(&mut self, key: KeyEvent) {
        let Some(machine) = self.machine.clone() else {
            return self.set_error("no home directory: nothing can be installed".into());
        };
        match key.code {
            KeyCode::Char('r') => {
                for tool in &mut self.tools {
                    tool.latest = None;
                }
                self.refresh_tooling(false);
                self.set_status("checking installed versions and the newest releases…".into());
            }
            KeyCode::Enter | KeyCode::Char('u') => {
                let name = self.tooling.selected_row().and_then(|r| self.tooling.table.cell(r, "TOOL")).map(String::from);
                let Some(row) = name.and_then(|n| self.tools.iter().position(|t| t.name() == n)) else { return };
                let tool = &self.tools[row];
                match tool.state(&machine) {
                    "system package" => self.set_status(format!("{} is a system package: update it with your package manager", tool.name())),
                    _ if !tool.needs_install(&machine) => self.set_status(format!("{} is up to date", tool.name())),
                    _ => self.plan_tooling(&[row]),
                }
            }
            KeyCode::Char('U') => {
                let rows: Vec<usize> = (0..self.tools.len()).filter(|&i| self.tools[i].needs_install(&machine)).collect();
                if rows.is_empty() {
                    return self.set_status("everything is installed and up to date".into());
                }
                self.plan_tooling(&rows);
            }
            _ => {}
        }
    }

    /// Asks before installing: the exact commands, plus the rc edit for the shell wiring.
    fn plan_tooling(&mut self, rows: &[usize]) {
        let Some(machine) = self.machine.clone() else { return };
        let mut commands = Vec::new();
        let mut shell = false;
        for &row in rows {
            let tool = &self.tools[row];
            if tool.kind == ToolKind::Shell {
                shell = true;
                continue;
            }
            match tool.install_command(&machine) {
                Ok(command) => commands.push(command),
                Err(e) => return self.set_error(format!("{}: {e}", tool.name())),
            }
        }
        let names = rows.iter().map(|&r| self.tools[r].name()).collect::<Vec<_>>().join(", ");
        let argv = (!commands.is_empty()).then(|| helpers::shell(&commands.join(" && ")));
        if !shell {
            if let Some(argv) = argv {
                self.confirm(format!("Install {names}?"), argv);
            }
            return;
        }
        let rc = machine.display(&machine.rc_file());
        let mut lines = vec![format!("kubewatcher writes its block into {rc}: kubeconfig_rebuild, ~/.local/bin on PATH, kx, kns")];
        if machine.has_oh_my_zsh() {
            lines.push("and adds kubectl to oh-my-zsh's plugins=(…) for kgp, klf, …".into());
        } else {
            lines.push("and the aliases k, kgp, kgs, kgd, kgn, kl, kd".into());
        }
        if let Some(argv) = &argv {
            lines.push("then runs:".into());
            lines.extend(helpers::shell_join(argv).split(" && ").map(|part| format!("  $ {part}")));
        }
        self.mode = Mode::ConfirmPlan { title: format!("Install {names}?"), lines, plan: Plan::Tooling { argv, shell } };
    }

    /// Adds kubewatcher's rc block (unless devsetup's is there) and the oh-my-zsh plugin.
    fn write_shell_wiring(&mut self) {
        let Some(machine) = self.machine.clone() else { return };
        let rc = machine.rc_file();
        let text = fs::read_to_string(&rc).unwrap_or_default();
        let outcome = match Tool::wired_rc(&machine, &text) {
            None => Ok("already wired".to_string()),
            Some(wired) => fs::write(&rc, wired).map(|()| "block written".to_string()).map_err(|e| e.to_string()),
        };
        let shown = helpers::shell(&format!("# kubewatcher: write its block into {}", rc.display()));
        let rc = machine.display(&rc);
        match self.journal_step(shown, "Tooling ▸ shell wiring", outcome) {
            Ok(_) => self.set_status(format!("✔ shell wired in {rc}: open a new shell or run `source {rc}`")),
            Err(e) => self.set_error(format!("could not write {rc}: {e}")),
        }
    }

    fn allowed_while_locked(&self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q' | '?' | 'j' | 'k' | 'g' | 'G' | 'T' | 'J') => true,
            KeyCode::Esc | KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End => true,
            KeyCode::Enter | KeyCode::Char('u' | 'U' | 'r') => self.tab == Tab::Tooling,
            _ => false,
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
            Tab::Tooling => &mut self.tooling.filter,
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
            Tab::Tooling => self.tooling.clamp(),
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
                match self.host.background(&argv) {
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
        match Theme::save_choice(self.config_dir.as_deref(), &name) {
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
                let [table, note] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(body);
                self.contexts.render(frame, table, " Contexts".into(), &theme, |row| {
                    row.get(1).is_some_and(|name| *name == current).then_some(current_style)
                });
                let split = if self.files_to_split().is_empty() { "" } else { "  ·  some files hold several contexts: S splits them" };
                let note_text = format!(" one file per context in ~/.kube/config.d{split}");
                frame.render_widget(Paragraph::new(note_text).style(theme.fg(theme.muted)), note);
            }
            Tab::Tooling => {
                let state = self.tooling.table.column("STATE");
                let (missing, outdated) = (theme.fg(theme.danger), theme.fg(theme.warning));
                self.tooling.render(frame, body, " Tooling".into(), &theme, |row| match state.and_then(|i| row.get(i)).map(String::as_str) {
                    Some("missing") => Some(missing),
                    Some("update available") => Some(outdated),
                    _ => None,
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
            Mode::Command => {
                let mut spans = vec![
                    Span::styled(":", theme.fg(theme.accent).bold()),
                    Span::raw(self.input.as_str()),
                    Span::styled("█", theme.fg(theme.accent)),
                ];
                // Show what an alias really runs, so the full command is learned along the way.
                let expanded = helpers::expand_alias(self.input.trim_start_matches('!'));
                if expanded != self.input.trim_start_matches('!') {
                    spans.push(Span::styled(format!("   → {expanded}"), theme.fg(theme.muted)));
                }
                Line::from(spans)
            }
            Mode::Filter => {
                let filter = match self.tab {
                    Tab::Contexts => &self.contexts.filter,
                    Tab::Resources => &self.resources.filter,
                    Tab::Helm => &self.helm.filter,
                    Tab::Flux => &self.flux.filter,
                    Tab::Tooling => &self.tooling.filter,
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
            (Mode::AddContext(_), ..) => &[("Tab", "next field"), ("Enter", "next / save"), ("Esc", "cancel")],
            (Mode::ImportK3s(_), ..) => &[("Tab", "next field"), ("Enter", "next / fetch"), ("Esc", "cancel")],
            (Mode::ConfirmPlan { .. }, ..) => &[("y", "do it"), ("n", "cancel")],
            (Mode::Blocked { .. }, ..) => &[("Enter", "open Tooling"), ("q", "quit")],
            (_, _, Tab::Contexts) => {
                resource_hints = [("Enter", "use context"), ("a", "add"), ("K", "import k3s"), ("x", "remove")]
                    .into_iter()
                    .chain((!self.files_to_split().is_empty()).then_some(("S", "one file per context")))
                    .chain([("i", "cluster-info"), ("v", "version")])
                    .collect::<Vec<_>>();
                &resource_hints
            }
            (_, _, Tab::Tooling) => &[("Enter/u", "install/update"), ("U", "all missing/outdated"), ("r", "re-check")],
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
            Mode::AddContext(form) => {
                let file = match (&self.machine, form.name()) {
                    (Some(machine), name) if !name.is_empty() => machine.display(&helpers::context_file_path(&machine.config_d(), name)),
                    _ => "~/.kube/config.d/<name>.yaml".into(),
                };
                form.render(frame, area, &file, theme);
            }
            Mode::ImportK3s(form) => {
                let file = match (&self.machine, form.name()) {
                    (Some(machine), name) if !name.is_empty() => machine.display(&helpers::context_file_path(&machine.config_d(), name)),
                    _ => "~/.kube/config.d/<name>.yaml".into(),
                };
                form.render(frame, area, &file, theme);
            }
            Mode::ConfirmPlan { title, lines, .. } => {
                let mut text = vec![Line::from(title.as_str()).bold(), Line::from("")];
                text.extend(lines.iter().map(|l| Line::styled(l.as_str(), theme.fg(theme.warning))));
                text.push(Line::from(""));
                text.push(Line::from(vec![Span::styled(" y ", theme.badge(theme.danger)), Span::raw(" do it   "), Span::styled(" n ", theme.chip()), Span::raw(" cancel")]));
                let height = text.len() as u16 + 2;
                Self::popup(frame, area, " Confirm ", theme, theme.danger, 100, height, Paragraph::new(text).wrap(Wrap { trim: false }));
            }
            Mode::Blocked { missing } => {
                let mut text = vec![Line::from("kubewatcher needs these and can't be used until everything is installed:").bold(), Line::from("")];
                text.extend(missing.iter().map(|m| Line::styled(format!("  ✘ {m}"), theme.fg(theme.danger))));
                text.push(Line::from(""));
                text.push(Line::styled("Enter opens Tooling, where each one is installed with the commands shown first · q quits", theme.fg(theme.muted)));
                let height = text.len() as u16 + 2;
                Self::popup(frame, area, " Missing tools ", theme, theme.danger, 90, height, Paragraph::new(text).wrap(Wrap { trim: false }));
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
                lines.push(Line::styled(
                    format!("  {} oh-my-zsh style aliases (kgp, klf, kgpa, ked, ...): Cheatsheet (6), filter /alias", helpers::ALIASES.len()),
                    theme.fg(theme.muted),
                ));
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
  1-7 / Tab      switch tab              n        pick namespace (Tab saves it)
  /              filter current list     w        auto-refresh every 2s
  :              run any command         R / F5   refresh
  J              toggle command strip    P        manage port-forwards      q  quit
  T              next color theme (files in ~/.config/kubewatcher/themes/, choice in config.toml)
CONTEXTS (1)
  Enter use context   a add (name, server, namespace, client cert + key data)   x remove (own file)
  K import from k3s: name, IP, user → ssh -t <user>@<ip> sudo cat /etc/rancher/k3s/k3s.yaml, keeps cert + key
  one file per context in ~/.kube/config.d · S splits files holding several (backup first)
TOOLING (5)
  kubectl, kubectx/kubens, fzf, helm, flux into ~/.local/bin, kubeconfig layout, shell wiring
  Enter/u install or update one · U everything missing or outdated · r re-check
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::rc::Rc;

    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyEventState;

    use super::*;

    const CONTEXTS: &str = "\
CURRENT   NAME      CLUSTER   AUTHINFO   NAMESPACE
*         prod      prod      admin      shop
          staging   staging   admin
";
    const PODS: &str = "\
NAME    READY   STATUS    RESTARTS   AGE
web-1   1/1     Running   0          1d
";
    const DEPLOYMENTS: &str = "\
NAME   READY   UP-TO-DATE   AVAILABLE   AGE
web    2/3     3            2           1d
";
    const SERVICES: &str = "\
NAME   TYPE        CLUSTER-IP   PORT(S)    AGE
web    ClusterIP   10.0.0.9     8080/TCP   1d
";
    const REPLICASETS: &str = "\
NAME      DESIRED   CURRENT   READY   AGE
web-abc   3         3         3       1d
";
    const NODES: &str = "\
NAME       STATUS   ROLES
worker-1   Ready    <none>
";
    const HELM: &str = r#"[{"name":"web","namespace":"shop","revision":"3","status":"deployed","chart":"web-1.0"}]"#;
    const FLUX: &str = "NAMESPACE\tNAME\tREVISION\tSUSPENDED\tREADY\tMESSAGE\n\
flux-system\tapps\tmain@sha1:abc\tTrue\tTrue\tApplied\n\
infra\tcore\tmain@sha1:abc\tFalse\tFalse\tfailed\n";

    /// What the scripted host has been asked to do, and how it should answer.
    #[derive(Default)]
    struct Script {
        /// Every command started, as "<how>: <command>".
        commands: Vec<String>,
        /// Canned `capture` results, by command prefix.
        replies: Vec<(&'static str, Result<String, String>)>,
        events: VecDeque<Event>,
        /// Starting streams and background processes fails.
        spawn_fails: bool,
        /// Shell script run for background (port-forward) processes.
        background: &'static str,
    }

    struct FakeHost(Rc<RefCell<Script>>);

    impl FakeHost {
        fn record(&self, how: &str, argv: &[String]) {
            self.0.borrow_mut().commands.push(format!("{how}: {}", helpers::shell_join(argv)));
        }

        fn spawn_error(&self) -> io::Result<()> {
            if self.0.borrow().spawn_fails { Err(io::Error::other("no such program")) } else { Ok(()) }
        }
    }

    impl Host for FakeHost {
        fn capture(&mut self, argv: Vec<String>, target: Target, journal: u64, tx: Sender<Msg>) {
            self.record("capture", &argv);
            let command = helpers::shell_join(&argv);
            let reply = self.0.borrow().replies.iter().find(|(prefix, _)| command.starts_with(prefix)).map(|(_, r)| r.clone());
            if let Some(result) = reply {
                tx.send(Msg::Output { target, journal, result }).unwrap();
            }
        }

        fn run(&mut self, argv: &[String]) -> Result<String, String> {
            self.record("run", argv);
            let command = helpers::shell_join(argv);
            let reply = self.0.borrow().replies.iter().find(|(prefix, _)| command.starts_with(prefix)).map(|(_, r)| r.clone());
            reply.unwrap_or_else(|| Err("no scripted reply".into()))
        }

        fn stream(&mut self, argv: &[String], generation: u64, tx: &Sender<Msg>) -> io::Result<Child> {
            self.record("stream", argv);
            self.spawn_error()?;
            helpers::spawn_stream(&helpers::shell("echo streamed; sleep 30"), generation, tx)
        }

        fn background(&mut self, argv: &[String]) -> io::Result<Child> {
            self.record("background", argv);
            self.spawn_error()?;
            helpers::spawn_background(&helpers::shell(self.0.borrow().background))
        }

        fn next_event(&mut self, _timeout: Duration) -> io::Result<Option<Event>> {
            let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
            Ok(Some(self.0.borrow_mut().events.pop_front().unwrap_or(ctrl_c)))
        }

        fn interactive(&mut self, argv: &[String], _pause: bool) -> io::Result<Result<(), String>> {
            self.record("interactive", argv);
            Ok(if helpers::shell_join(argv).contains("fail") { Err("exit status: 1".into()) } else { Ok(()) })
        }

        fn interactive_capture(&mut self, argv: &[String], _hide_from: &str) -> io::Result<Result<String, String>> {
            self.record("interactive", argv);
            let command = helpers::shell_join(argv);
            let reply = self.0.borrow().replies.iter().find(|(prefix, _)| command.starts_with(prefix)).map(|(_, r)| r.clone());
            Ok(reply.unwrap_or_else(|| Err("no scripted reply".into())))
        }
    }

    fn app() -> (App, Rc<RefCell<Script>>) {
        let script = Rc::new(RefCell::new(Script { background: "sleep 30", ..Script::default() }));
        (App::with_host(Box::new(FakeHost(script.clone())), None, None, None), script)
    }

    /// An app whose context (prod, namespace shop) is known and whose pods are listed.
    fn loaded() -> (App, Rc<RefCell<Script>>) {
        let (mut app, script) = app();
        app.on_output(Target::Contexts, Ok(CONTEXTS.into()));
        show(&mut app, "pods", PODS);
        (app, script)
    }

    fn show(app: &mut App, kind: &str, text: &str) {
        app.tab = Tab::Resources;
        app.kind_index = KINDS.iter().position(|k| k.name == kind).unwrap();
        app.resources.reset();
        app.resources.set_result(Ok(KubeTable::parse(text)));
    }

    fn last(script: &Rc<RefCell<Script>>) -> String {
        script.borrow().commands.last().cloned().unwrap_or_default()
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn keys(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn command(app: &mut App, line: &str) {
        press(app, KeyCode::Char(':'));
        keys(app, line);
        press(app, KeyCode::Enter);
    }

    fn status(app: &App) -> String {
        app.status.as_ref().map(|(message, _)| message.clone()).unwrap_or_default()
    }

    fn draw_sized(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().to_string()
    }

    fn draw(app: &mut App) -> String {
        draw_sized(app, 160, 40)
    }

    #[test]
    fn run_loop_drives_keys_messages_and_the_terminal() {
        let (app, script) = app();
        {
            let mut s = script.borrow_mut();
            s.replies = vec![("kubectl config get-contexts", Ok(CONTEXTS.into())), ("kubectl get pods", Ok(PODS.into()))];
            let key = |c: char| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            let release = Event::Key(KeyEvent {
                code: KeyCode::Char('q'),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Release,
                state: KeyEventState::NONE,
            });
            s.events.extend([Event::Resize(160, 40), release]);
            for line in [":!kubectl edit deploy web", ":!fail", "f8080:80", ":kubectl logs -f web-1"] {
                s.events.extend(line.chars().map(key));
                s.events.push_back(enter.clone());
            }
        }
        let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
        app.run(&mut terminal).unwrap();

        let commands = script.borrow().commands.join("\n");
        assert!(commands.contains("interactive: kubectl edit deploy web"), "{commands}");
        assert!(commands.contains("interactive: fail"));
        assert!(commands.contains("background: kubectl port-forward pods/web-1 8080:80 -n shop"));
        assert!(commands.contains("stream: kubectl logs -f web-1"));
    }

    #[test]
    fn contexts_set_the_namespace_once_and_can_be_switched() {
        let (mut app, script) = app();
        app.on_output(Target::Contexts, Ok(CONTEXTS.into()));
        assert_eq!((app.current_context.as_str(), app.namespace.as_deref()), ("prod", Some("shop")));
        let commands = script.borrow().commands.join("\n");
        assert!(commands.contains("kubectl get pods -n shop -o wide"));
        assert!(commands.contains("helm list --all -o json -n shop"));
        assert!(commands.contains("flux get kustomizations -A"));

        // Later refreshes keep the namespace you picked.
        app.namespace = Some("other".into());
        app.on_output(Target::Contexts, Err("connection refused".into()));
        assert_eq!(app.namespace.as_deref(), Some("other"));

        press(&mut app, KeyCode::Char('1'));
        app.on_output(Target::Contexts, Ok(CONTEXTS.into()));
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(last(&script), "capture: kubectl cluster-info");
        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('v'));
        assert_eq!(last(&script), "capture: kubectl version");
        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('z'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert!(script.borrow().commands.iter().any(|c| c == "capture: kubectl config use-context staging"));
        assert!(!app.namespace_initialized);

        // A context without a namespace means "default", like kubectl.
        let (mut app, _) = self::app();
        app.on_output(Target::Contexts, Ok("CURRENT   NAME\n*         kind\n".into()));
        assert_eq!(app.namespace.as_deref(), Some("default"));
        app.tab = Tab::Contexts;
        app.contexts.set_result(Ok(KubeTable::default()));
        press(&mut app, KeyCode::Enter);
    }

    /// A machine in a temp dir; with `ready`, every tool, the kubeconfig layout and the shell wiring are there.
    fn machine_app(name: &str, ready: bool) -> (App, Rc<RefCell<Script>>, Machine, PathBuf) {
        let dir = helpers::test_dir(name);
        let machine = Machine::new(dir.join("home"), OsString::new(), "bash");
        fs::create_dir_all(&machine.home).unwrap();
        if ready {
            fs::create_dir_all(machine.local_bin()).unwrap();
            for program in ["kubectl", "kubectx", "kubens", "fzf", "helm", "flux"] {
                fake_program(&machine.local_bin().join(program));
            }
            fs::create_dir_all(machine.config_d()).unwrap();
            fs::write(machine.kube_config(), "").unwrap();
            fs::write(machine.rc_file(), "# >>> devsetup >>>\n").unwrap();
        }
        let script = Rc::new(RefCell::new(Script { background: "sleep 30", ..Script::default() }));
        let app = App::with_host(Box::new(FakeHost(script.clone())), None, None, Some(machine.clone()));
        (app, script, machine, dir)
    }

    fn fake_program(path: &Path) {
        fs::write(path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn drain(app: &mut App) {
        while let Ok(msg) = app.rx.try_recv() {
            app.handle_msg(msg);
        }
    }

    #[test]
    fn missing_tools_lock_everything_but_tooling() {
        let (mut app, script, machine, dir) = machine_app("app-tooling", false);
        assert!(app.locked && app.tab == Tab::Tooling);
        assert!(draw(&mut app).contains("Missing tools"));
        press(&mut app, KeyCode::Char('1'));
        assert!(matches!(app.mode, Mode::Blocked { .. }));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal) && app.tab == Tab::Tooling);
        press(&mut app, KeyCode::Char('1'));
        assert!(app.tab == Tab::Tooling && status(&app).contains("tools are missing"));
        press(&mut app, KeyCode::Char(':'));
        assert!(matches!(app.mode, Mode::Normal));

        script.borrow_mut().replies = vec![
            ("curl -fsSL https://dl.k8s.io/release/stable.txt", Ok("v1.34.1\n".into())),
            ("curl -fsSLI", Ok("https://github.com/x/y/releases/tag/v1.0.0".into())),
        ];
        app.refresh_tooling(true);
        drain(&mut app);
        assert_eq!(app.tools[0].latest.as_deref(), Some("v1.34.1"));
        assert_eq!(app.tools[3].latest.as_deref(), Some("v1.0.0"));
        let screen = draw(&mut app);
        assert!(screen.contains("kubectx + kubens") && screen.contains("missing") && screen.contains("~/.bashrc"));

        // Everything at once: the rc block is written, the downloads go through confirm + journal.
        press(&mut app, KeyCode::Char('U'));
        assert!(draw(&mut app).contains("kubewatcher writes its block into ~/.bashrc"));
        script.borrow_mut().commands.clear();
        press(&mut app, KeyCode::Char('y'));
        assert!(fs::read_to_string(machine.rc_file()).unwrap().contains("# >>> kubewatcher >>>"));
        if std::env::consts::ARCH == "x86_64" {
            let command = last(&script);
            assert!(command.starts_with("capture: mkdir -p ") && command.contains("install -m 755 kubectl"));
            assert!(command.contains("printf '%s\\n' 'apiVersion: v1'"));
        }

        // Once everything is there, the re-check unlocks the app and loads the contexts.
        fs::create_dir_all(machine.local_bin()).unwrap();
        for program in ["kubectl", "kubectx", "kubens", "fzf", "helm", "flux"] {
            fake_program(&machine.local_bin().join(program));
        }
        fs::create_dir_all(machine.config_d()).unwrap();
        fs::write(machine.kube_config(), "").unwrap();
        script.borrow_mut().commands.clear();
        app.refresh_current(true);
        assert!(!app.locked && status(&app).contains("ready"));
        assert!(script.borrow().commands.iter().any(|c| c == "capture: kubectl config get-contexts"));
        assert!(script.borrow().commands.iter().any(|c| c.ends_with("kubectl version --client")));
        press(&mut app, KeyCode::Char('1'));
        assert!(app.tab == Tab::Contexts);

        app.tab = Tab::Tooling;
        app.tools[0].installed = Some("1.34.1".into());
        app.update_tooling_table();
        app.tooling.state.select(Some(0));
        press(&mut app, KeyCode::Enter);
        assert_eq!(status(&app), "kubectl is up to date");
        press(&mut app, KeyCode::Char('r'));
        assert!(status(&app).starts_with("checking"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn k3s_import_reads_k3s_yaml_over_ssh_and_logs_no_secrets() {
        let (mut app, script, machine, _dir) = machine_app("app-k3s", true);
        assert!(!app.locked);
        app.on_output(Target::Contexts, Ok(CONTEXTS.into()));
        app.tab = Tab::Contexts;

        // A taken name is refused before any ssh.
        press(&mut app, KeyCode::Char('K'));
        for value in ["prod", "10.5.15.190", "additive"] {
            keys(&mut app, value);
            press(&mut app, KeyCode::Enter);
        }
        assert!(status(&app).contains("already exists") && matches!(app.mode, Mode::ImportK3s(_)), "{}", status(&app));
        press(&mut app, KeyCode::Esc);

        press(&mut app, KeyCode::Char('K'));
        for value in ["teststand3", "10.5.15.190"] {
            keys(&mut app, value);
            press(&mut app, KeyCode::Enter);
        }
        keys(&mut app, "additive");
        assert!(draw(&mut app).contains("ssh -t additive@10.5.15.190 sudo cat /etc/rancher/k3s/k3s.yaml"));
        press(&mut app, KeyCode::Enter);
        let mut job = app.pending_interactive.take().unwrap();
        assert_eq!(helpers::shell_join(&job.argv), "ssh -t additive@10.5.15.190 'sudo cat /etc/rancher/k3s/k3s.yaml'");

        let k3s_yaml = "[sudo] password for additive: \r\napiVersion: v1\r\n    server: https://0.0.0.0:6443\r\n    client-certificate-data: Q0VSVA==\r\n    client-key-data: S0VZ\r\n";
        script.borrow_mut().replies = vec![("ssh -t", Ok(k3s_yaml.into()))];
        let output = app.host.interactive_capture(&job.argv, "apiVersion:").unwrap();
        app.finish_k3s_import(job.journal, &job.k3s.take().unwrap(), output);
        let file = machine.config_d().join("teststand3.yaml");
        let ran = last(&script);
        assert!(ran.contains("set-cluster teststand3 --server=https://10.5.15.190:6443 --insecure-skip-tls-verify=true"), "{ran}");
        assert!(ran.contains("set users.teststand3.client-certificate-data Q0VSVA==") && ran.contains("set users.teststand3.client-key-data S0VZ"), "{ran}");
        assert!(ran.contains(&format!("--kubeconfig={}", file.display())), "{ran}");
        assert!(!status(&app).contains("Q0VSVA"));
        app.tab = Tab::Journal;
        let journal = draw(&mut app);
        assert!(journal.contains("import teststand3") && !journal.contains("Q0VSVA") && !journal.contains("S0VZ"), "{journal}");

        // Without the yaml (wrong sudo password) nothing is written.
        script.borrow_mut().commands.clear();
        app.finish_k3s_import(0, &K3sForm::default(), Ok("Sorry, try again.\r\n".into()));
        assert!(status(&app).contains("no server") && script.borrow().commands.is_empty());
    }

    #[test]
    fn contexts_get_a_file_each_and_can_be_split_and_removed() {
        let (mut app, script, machine, dir) = machine_app("app-contexts", true);
        assert!(!app.locked);
        let config = machine.kube_config();
        let config_d = machine.config_d();
        let view = |file: &Path| format!("kubectl config view --kubeconfig={} -o json", file.display());
        let both = r#"{"current-context":"prod","contexts":[{"name":"prod"},{"name":"staging"}]}"#;
        script.borrow_mut().replies = vec![
            (Box::leak(view(&config).into_boxed_str()), Ok(both.into())),
            (Box::leak(format!("kubectl config view --kubeconfig={} --context=prod", config.display()).into_boxed_str()), Ok("apiVersion: v1\ncurrent-context: prod\nclusters: []\n".into())),
            (Box::leak(format!("kubectl config view --kubeconfig={} --context=staging", config.display()).into_boxed_str()), Ok("apiVersion: v1\nusers: []\n".into())),
        ];
        app.on_output(Target::Contexts, Ok(CONTEXTS.into()));
        app.tab = Tab::Contexts;
        let screen = draw(&mut app);
        assert!(screen.contains("FILE") && screen.contains("~/.kube/config") && screen.contains("S splits them"));

        // Adding: a taken name is refused; a new one gets its own file.
        press(&mut app, KeyCode::Char('a'));
        for value in ["prod", "10.0.0.5", "", "c3rt", "s3cret"] {
            keys(&mut app, value);
            press(&mut app, KeyCode::Tab);
        }
        press(&mut app, KeyCode::BackTab);
        press(&mut app, KeyCode::Enter);
        assert!(status(&app).contains("already exists") && matches!(app.mode, Mode::AddContext(_)));
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('a'));
        keys(&mut app, "lab");
        assert!(draw(&mut app).contains("~/.kube/config.d/lab.yaml"));
        for value in ["", "10.0.0.5", "", "c3rt", "s3cret"] {
            keys(&mut app, value);
            press(&mut app, KeyCode::Enter);
        }
        let kubeconfig = format!("--kubeconfig={}", config_d.join("lab.yaml").display());
        assert!(last(&script).contains(&format!("set-context lab --cluster=lab --user=lab --namespace=default {kubeconfig}")));
        app.status = None;
        assert!(!draw(&mut app).contains("s3cret"));

        // A context sharing a file cannot be removed; splitting backs up and writes one file each.
        app.contexts.state.select(Some(0));
        press(&mut app, KeyCode::Char('x'));
        assert!(status(&app).contains("shares ~/.kube/config"));
        press(&mut app, KeyCode::Char('S'));
        assert!(draw(&mut app).contains("config.d/prod.yaml, config.d/staging.yaml"));
        press(&mut app, KeyCode::Char('y'));
        assert!(status(&app).starts_with("✔ 2 context(s)"), "{}", status(&app));
        assert_eq!(fs::read_to_string(config_d.join("prod.yaml")).unwrap(), "apiVersion: v1\nclusters: []\n");
        assert_eq!(fs::read_to_string(config_d.join("staging.yaml")).unwrap(), "apiVersion: v1\nusers: []\n");
        assert!(fs::read_to_string(&config).unwrap().contains("current-context: \"prod\""));
        let backups = fs::read_dir(machine.home.join(".kube")).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("config.bak-")).count();
        assert_eq!(backups, 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(config_d.join("prod.yaml")).unwrap().permissions().mode() & 0o777, 0o600);
        }

        // Now prod has a file of its own and can be removed.
        script.borrow_mut().replies = vec![
            (Box::leak(view(&config).into_boxed_str()), Ok(r#"{"current-context":"prod","contexts":null}"#.into())),
            (Box::leak(view(&config_d.join("prod.yaml")).into_boxed_str()), Ok(r#"{"contexts":[{"name":"prod"}]}"#.into())),
        ];
        app.on_output(Target::Contexts, Ok(CONTEXTS.into()));
        assert!(app.kubeconfig.iter().any(|f| f.path == config_d.join("prod.yaml")), "KUBECONFIG is rebuilt from config.d");
        app.contexts.state.select(Some(0));
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(last(&script), format!("capture: rm {}", config_d.join("prod.yaml").display()));
        press(&mut app, KeyCode::Char('S'));
        assert_eq!(status(&app), "every context already has a file of its own");
        app.contexts.state.select(Some(1));
        press(&mut app, KeyCode::Char('x'));
        assert!(status(&app).contains("no kubeconfig file defines staging"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn namespace_picker_filters_selects_and_saves() {
        let (mut app, script) = loaded();
        app.on_output(Target::Namespaces, Ok("shop default kube-system".into()));
        assert_eq!(app.completer.namespaces, ["default", "kube-system", "shop"]);
        app.on_output(Target::Namespaces, Err("forbidden".into()));
        assert!(status(&app).contains("forbidden"));

        press(&mut app, KeyCode::Char('n'));
        assert!(draw(&mut app).contains("Namespace:"));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.namespace, None);
        assert_eq!(status(&app), "namespace: (all namespaces)");
        assert!(draw(&mut app).contains("ns: all"));

        press(&mut app, KeyCode::Char('n'));
        keys(&mut app, "kube");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.namespace.as_deref(), Some("kube-system"));
        assert_eq!(last(&script), "capture: kubectl config set-context --current --namespace=kube-system");

        press(&mut app, KeyCode::Char('n'));
        keys(&mut app, "all");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.namespace, None);

        press(&mut app, KeyCode::Char('n'));
        keys(&mut app, "nothing");
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn resource_keys_run_the_matching_kubectl_commands() {
        let (mut app, script) = loaded();
        for (key, expected) in [
            (KeyCode::Enter, "capture: kubectl describe pods web-1 -n shop"),
            (KeyCode::Char('y'), "capture: kubectl get pods web-1 -o yaml -n shop"),
            (KeyCode::Char('v'), "capture: kubectl get events --field-selector involvedObject.name=web-1 --sort-by=.lastTimestamp -n shop"),
            (KeyCode::Char('l'), "stream: kubectl logs -f --tail=500 --all-containers --prefix pods/web-1 -n shop"),
            (KeyCode::Char('L'), "capture: kubectl logs --previous --tail=500 --all-containers --prefix pods/web-1 -n shop"),
        ] {
            press(&mut app, key);
            assert_eq!(last(&script), expected);
            press(&mut app, KeyCode::Char('q'));
        }

        press(&mut app, KeyCode::Char('e'));
        assert!(app.pending_interactive.take().is_some_and(|job| job.argv[..4] == ["kubectl", "exec", "-it", "web-1"]));
        press(&mut app, KeyCode::Char('E'));
        assert_eq!(app.pending_interactive.take().unwrap().argv, argv(&["kubectl", "edit", "pods/web-1", "-n", "shop"]));

        press(&mut app, KeyCode::Char('x'));
        assert!(matches!(&app.mode, Mode::Confirm { message, .. } if message == "Delete pods/web-1?"));
        assert!(draw(&mut app).contains("$ kubectl delete pods/web-1 -n shop"));
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(status(&app), "cancelled");

        for (key, error) in [
            ('r', "rollout is not available for pods"),
            ('o', "rollout is not available for pods"),
            ('s', "pods aren't scaled directly"),
        ] {
            press(&mut app, KeyCode::Char(key));
            assert!(status(&app).contains(error), "{}", status(&app));
        }
        press(&mut app, KeyCode::Char('f'));
        assert!(matches!(app.mode, Mode::Prompt(Prompt::PortForward { .. })));
        assert_eq!(app.input, "");
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('z'));
        press(&mut app, KeyCode::Right);
        assert_eq!(app.kind().name, "deployments");
        press(&mut app, KeyCode::Char('['));
        press(&mut app, KeyCode::Left);
        assert_eq!(app.kind().name, "pods");
    }

    #[test]
    fn workload_keys_scale_restart_and_forward() {
        let (mut app, script) = loaded();
        show(&mut app, "deployments", DEPLOYMENTS);
        press(&mut app, KeyCode::Char('r'));
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(last(&script), "capture: kubectl rollout restart deployments/web -n shop");
        assert_eq!(status(&app), "$ kubectl rollout restart deployments/web -n shop");
        press(&mut app, KeyCode::Char('o'));
        assert_eq!(last(&script), "stream: kubectl rollout status -w deployments/web -n shop");
        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('e'));
        assert_eq!(status(&app), "✘ exec is not available for deployments");
        press(&mut app, KeyCode::Char('L'));

        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.input, "3");
        assert!(draw(&mut app).contains("Replicas for deployments/web:"));
        keys(&mut app, "x");
        press(&mut app, KeyCode::Enter);
        assert_eq!(status(&app), "✘ '3x' is not a replica count");
        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Backspace);
        keys(&mut app, "5");
        press(&mut app, KeyCode::Enter);
        assert_eq!(last(&script), "capture: kubectl scale deployments/web --replicas=5 -n shop");

        show(&mut app, "replicasets", REPLICASETS);
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.input, "3");
        press(&mut app, KeyCode::Esc);

        show(&mut app, "services", SERVICES);
        press(&mut app, KeyCode::Char('f'));
        assert_eq!(app.input, "8080:8080");
        assert!(draw(&mut app).contains("Ports for services/web (LOCAL:REMOTE):"));
        press(&mut app, KeyCode::Enter);
        assert_eq!(last(&script), "background: kubectl port-forward services/web 8080:8080 -n shop");
        assert!(status(&app).starts_with("port-forward started: localhost:8080 → services/web"));
        for (key, error) in [('l', "logs"), ('s', "scale"), ('r', "rollout")] {
            press(&mut app, KeyCode::Char(key));
            assert_eq!(status(&app), format!("✘ {error} is not available for services"));
        }

        show(&mut app, "nodes", NODES);
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(last(&script), "capture: kubectl describe nodes worker-1");
        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('f'));
        assert_eq!(status(&app), "✘ port-forward is not available for nodes");

        // Nothing selected: keys explain why nothing happens.
        show(&mut app, "secrets", "");
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(status(&app), "✘ no secrets selected");
        app.status = None;
        press(&mut app, KeyCode::Delete);
        assert_eq!(app.status, None);
    }

    #[test]
    fn all_namespaces_take_the_namespace_from_the_row() {
        let (mut app, script) = loaded();
        app.namespace = None;
        show(&mut app, "pods", "NAMESPACE   NAME    READY\nshop        web-1   1/1\n");
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(last(&script), "capture: kubectl describe pods web-1 -n shop");
        press(&mut app, KeyCode::Char('q'));
        show(&mut app, "pods", PODS);
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(last(&script), "capture: kubectl describe pods web-1");
    }

    #[test]
    fn prompts_validate_their_input() {
        let (mut app, script) = loaded();
        press(&mut app, KeyCode::Char('f'));
        keys(&mut app, "80:http");
        press(&mut app, KeyCode::Enter);
        assert_eq!(status(&app), "✘ '80:http' should look like LOCAL:REMOTE, e.g. 8080:80");

        script.borrow_mut().spawn_fails = true;
        press(&mut app, KeyCode::Char('f'));
        keys(&mut app, "8080:80");
        press(&mut app, KeyCode::Enter);
        assert_eq!(status(&app), "✘ port-forward failed: no such program");
        assert!(app.port_forwards.is_empty());

        app.mode = Mode::Prompt(Prompt::Rollback { release: "web".into(), namespace: None });
        assert!(draw(&mut app).contains("Roll back web to revision:"));
        keys(&mut app, "two");
        press(&mut app, KeyCode::Enter);
        assert_eq!(status(&app), "✘ 'two' is not a revision");
    }

    #[test]
    fn confirm_runs_edits_or_keeps_waiting() {
        let (mut app, script) = loaded();
        for key in [KeyCode::Char('y'), KeyCode::Char('Y'), KeyCode::Enter] {
            press(&mut app, KeyCode::Char('x'));
            press(&mut app, key);
            assert_eq!(last(&script), "capture: kubectl delete pods/web-1 -n shop");
            script.borrow_mut().commands.clear();
        }
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Char('z'));
        assert!(matches!(app.mode, Mode::Confirm { .. }));
        assert!(draw(&mut app).contains("edit in prompt first"));
        press(&mut app, KeyCode::Char('e'));
        assert!(matches!(app.mode, Mode::Command));
        assert_eq!(app.input, "kubectl delete pods/web-1 -n shop");
    }

    #[test]
    fn action_results_go_to_the_status_bar() {
        let (mut app, script) = loaded();
        app.on_output(Target::Action, Ok("\npod \"web-1\" deleted\n".into()));
        assert_eq!(status(&app), "✔ pod \"web-1\" deleted");
        assert_eq!(last(&script), "capture: kubectl get pods -n shop -o wide");
        app.on_output(Target::Action, Ok(String::new()));
        assert_eq!(status(&app), "✔ done");
        app.on_output(Target::Action, Err("Error from server\n\n  (Forbidden)".into()));
        assert_eq!(status(&app), "✘ Error from server ⏎ (Forbidden)");
        assert!(draw(&mut app).contains("Forbidden"));

        // Results for an old request (kind or namespace changed since) are dropped.
        app.on_output(Target::Resources(999), Ok(DEPLOYMENTS.into()));
        app.on_output(Target::Resources(app.resources.request), Ok(DEPLOYMENTS.into()));
        assert_eq!(app.resources.table.rows[0][0], "web");
        app.on_output(Target::Completions, Ok("pod/web-1\n".into()));
        app.on_output(Target::Completions, Err("forbidden".into()));
    }

    #[test]
    fn helm_keys_inspect_roll_back_and_uninstall() {
        let (mut app, script) = loaded();
        app.on_output(Target::Helm(app.helm.request), Ok(HELM.into()));
        assert_eq!(app.completer.releases, ["web"]);
        press(&mut app, KeyCode::Char('3'));
        assert!(draw(&mut app).contains("deployed"));
        for (key, expected) in [
            (KeyCode::Enter, "capture: helm status web -n shop"),
            (KeyCode::Char('s'), "capture: helm status web -n shop"),
            (KeyCode::Char('v'), "capture: helm get values web -n shop"),
            (KeyCode::Char('V'), "capture: helm get values web --all -n shop"),
            (KeyCode::Char('m'), "capture: helm get manifest web -n shop"),
            (KeyCode::Char('N'), "capture: helm get notes web -n shop"),
            (KeyCode::Char('h'), "capture: helm history web -n shop"),
        ] {
            press(&mut app, key);
            assert_eq!(last(&script), expected);
            press(&mut app, KeyCode::Char('q'));
        }
        press(&mut app, KeyCode::Char('b'));
        assert_eq!(app.input, "2");
        press(&mut app, KeyCode::Enter);
        assert_eq!(last(&script), "capture: helm rollback web 2 -n shop");
        press(&mut app, KeyCode::Char('x'));
        assert!(matches!(&app.mode, Mode::Confirm { message, .. } if message == "Uninstall helm release web?"));
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(last(&script), "capture: helm uninstall web -n shop");
        press(&mut app, KeyCode::Char('z'));

        // No namespace or revision in the output: fall back to the current namespace and revision 1.
        app.on_output(Target::Helm(app.helm.request), Ok(r#"[{"name":"db"}]"#.into()));
        press(&mut app, KeyCode::Char('b'));
        assert_eq!(app.input, "1");
        press(&mut app, KeyCode::Esc);
        app.on_output(Target::Helm(999), Ok(HELM.into()));
        app.on_output(Target::Helm(app.helm.request), Err("Error: Kubernetes cluster unreachable".into()));
        app.helm.set_result(Ok(KubeTable::default()));
        script.borrow_mut().commands.clear();
        press(&mut app, KeyCode::Char('s'));
        assert!(script.borrow().commands.is_empty());
    }

    #[test]
    fn flux_keys_suspend_resume_reconcile_and_inspect() {
        let (mut app, script) = loaded();
        press(&mut app, KeyCode::Char('4'));
        app.on_output(Target::Flux(app.flux.request), Ok(FLUX.into()));
        assert_eq!(app.completer.flux_names, ["apps", "core"]);
        assert!(draw(&mut app).contains("Flux kustomizations (all namespaces)"));

        press(&mut app, KeyCode::Char('C'));
        assert_eq!(last(&script), "capture: flux check");
        press(&mut app, KeyCode::Char('q'));

        press(&mut app, KeyCode::Char('S'));
        let all = "flux suspend kustomization --all -n flux-system && flux suspend kustomization --all -n infra";
        assert!(matches!(&app.mode, Mode::Confirm { message, .. } if message.starts_with("Suspend ALL kustomizations")));
        assert!(draw(&mut app).contains("$ flux suspend kustomization --all -n infra"));
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(last(&script), format!("capture: {all}"));
        press(&mut app, KeyCode::Char('U'));
        assert!(matches!(&app.mode, Mode::Confirm { message, .. } if message.starts_with("Resume ALL kustomizations")));
        press(&mut app, KeyCode::Char('n'));

        for (key, expected) in [
            ('d', "capture: kubectl describe kustomizations.kustomize.toolkit.fluxcd.io apps -n flux-system"),
            ('y', "capture: kubectl get kustomizations.kustomize.toolkit.fluxcd.io apps -o yaml -n flux-system"),
            ('r', "stream: flux reconcile kustomization apps -n flux-system --with-source"),
            ('e', "capture: flux events --for Kustomization/apps -n flux-system"),
            ('l', "stream: flux logs --follow --tail=200 --kind=Kustomization --name=apps -n flux-system"),
            ('t', "capture: flux tree kustomization apps -n flux-system"),
        ] {
            press(&mut app, KeyCode::Char(key));
            assert_eq!(last(&script), expected);
            press(&mut app, KeyCode::Char('q'));
        }
        for (key, expected) in [('s', "capture: flux suspend kustomization apps -n flux-system"), ('u', "capture: flux resume kustomization apps -n flux-system")] {
            press(&mut app, KeyCode::Char(key));
            press(&mut app, KeyCode::Char('y'));
            assert_eq!(last(&script), expected);
        }
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('z'));

        // Already the first kind: nothing to reload.
        script.borrow_mut().commands.clear();
        press(&mut app, KeyCode::Left);
        assert!(script.borrow().commands.is_empty());
        press(&mut app, KeyCode::Right);
        assert_eq!(last(&script), "capture: flux get helmreleases -A");
        press(&mut app, KeyCode::Char(']'));
        press(&mut app, KeyCode::Char('['));
        press(&mut app, KeyCode::Char(']'));
        assert_eq!(app.flux_kind().label, "git sources");

        press(&mut app, KeyCode::Char('S'));
        assert_eq!(status(&app), "✘ no git sources listed");
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(status(&app), "✘ no git sources selected");
        app.status = None;
        press(&mut app, KeyCode::Delete);
        assert_eq!(app.status, None);

        app.on_output(Target::Flux(999), Ok(FLUX.into()));
        app.on_output(Target::Flux(app.flux.request), Ok("NAMESPACE\tNAME\nflux-system\trepo\n".into()));
        assert!(draw(&mut app).contains("repo"));
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(last(&script), "stream: flux reconcile source git repo -n flux-system");
        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(status(&app), "✘ flux tree only works for kustomizations");
    }

    #[test]
    fn journal_and_cheatsheet_load_commands_into_the_prompt() {
        let (mut app, _script) = loaded();
        press(&mut app, KeyCode::Char('6'));
        assert!(draw(&mut app).contains("Journal"));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Command));
        assert_eq!(app.input, "flux get kustomizations -A");
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('c'));
        press(&mut app, KeyCode::Char('a'));
        assert!(app.journal.changes_only && app.journal.hide_auto);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        press(&mut app, KeyCode::Char('z'));

        press(&mut app, KeyCode::Char('7'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.input, "kubectl config get-contexts");
        press(&mut app, KeyCode::Esc);
        app.cheatsheet.filter = "get all -n".into();
        app.cheatsheet.clamp();
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.input, "kubectl get all -n shop");
        press(&mut app, KeyCode::Esc);
        app.namespace = None;
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.input, "kubectl get all -n <ns>");
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('z'));
    }

    #[test]
    fn filter_mode_works_on_every_tab() {
        let (mut app, _script) = loaded();
        for tab in '1'..='7' {
            press(&mut app, KeyCode::Char(tab));
            press(&mut app, KeyCode::Char('/'));
            keys(&mut app, "web");
            assert!(draw(&mut app).contains("/web"), "tab {tab}");
            press(&mut app, KeyCode::Down);
            press(&mut app, KeyCode::Up);
            press(&mut app, KeyCode::Backspace);
            press(&mut app, KeyCode::Enter);
            assert!(matches!(app.mode, Mode::Normal));
            press(&mut app, KeyCode::Char('/'));
            press(&mut app, KeyCode::Esc);
        }
        assert!(app.resources.filter.is_empty() && app.cheatsheet.filter.is_empty());
    }

    #[test]
    fn command_prompt_completes_remembers_and_runs() {
        let (mut app, script) = loaded();
        press(&mut app, KeyCode::Char(':'));
        let fetches = |script: &Rc<RefCell<Script>>| script.borrow().commands.iter().filter(|c| c.contains("-o name")).count();
        assert_eq!(fetches(&script), 1);
        assert!(last(&script).starts_with("capture: kubectl get namespaces"));
        keys(&mut app, "klf web");
        assert!(draw(&mut app).contains("→ kubectl logs -f web"));
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char(':'));
        assert_eq!(fetches(&script), 1);
        press(&mut app, KeyCode::Esc);
        app.completions_fetched = Instant::now().checked_sub(COMPLETION_TTL);
        app.namespaces = vec!["shop".into()];
        press(&mut app, KeyCode::Char(':'));
        assert_eq!(fetches(&script), 2);

        app.on_output(Target::Completions, Ok("pod/web-1\npod/web-2\n".into()));
        keys(&mut app, "kubectl logs web");
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.input, "kubectl logs web-1");
        press(&mut app, KeyCode::BackTab);
        assert_eq!(app.input, "kubectl logs web-2");
        assert!(draw(&mut app).contains("Tab (2)"));
        press(&mut app, KeyCode::Char('u'));
        app.input.clear();
        keys(&mut app, "kubectl ");
        for _ in 0..12 {
            press(&mut app, KeyCode::Tab);
        }
        assert!(draw(&mut app).contains("Tab (29)"));
        app.completer.reset();
        app.input = "zzz".into();
        assert!(!draw(&mut app).contains("Tab ("));

        // History: Up/Down walk through earlier lines.
        app.input.clear();
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "");
        keys(&mut app, "kubectl get pods");
        press(&mut app, KeyCode::Enter);
        assert_eq!(last(&script), "capture: kubectl get pods");
        press(&mut app, KeyCode::Char('q'));
        command(&mut app, "kubectl get pods");
        press(&mut app, KeyCode::Char('q'));
        command(&mut app, "kubectl get svc -w");
        assert_eq!(last(&script), "stream: kubectl get svc -w");
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.history, ["kubectl get pods", "kubectl get svc -w"]);
        press(&mut app, KeyCode::Char(':'));
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.input, "kubectl get pods");
        press(&mut app, KeyCode::Down);
        assert_eq!(app.input, "kubectl get svc -w");
        press(&mut app, KeyCode::Down);
        assert_eq!(app.input, "");
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn command_lines_pick_the_right_way_to_run() {
        let (mut app, script) = loaded();
        let interactive = |app: &mut App| app.pending_interactive.take().map(|job| helpers::shell_join(&job.argv));
        command(&mut app, "!kgp");
        assert_eq!(interactive(&mut app).as_deref(), Some("kubectl get pods"));
        command(&mut app, "kubectl exec -it web-1 -- sh");
        assert_eq!(interactive(&mut app).as_deref(), Some("kubectl exec -it web-1 -- sh"));
        command(&mut app, "ked web");
        assert_eq!(interactive(&mut app).as_deref(), Some("kubectl edit deployment web"));

        command(&mut app, "watch");
        assert_eq!(status(&app), "✘ usage: watch [-n SECONDS] <command>");
        for (line, interval) in [("watch -n 5 kgp", 5.0), ("watch -n 0.1 kubectl get pods", 0.5), ("watch kubectl get pods", 2.0)] {
            command(&mut app, line);
            assert_eq!(last(&script), "capture: kubectl get pods");
            assert_eq!(app.viewer.as_ref().and_then(|v| v.watch), Some(Duration::from_secs_f64(interval)));
            press(&mut app, KeyCode::Char('q'));
        }
    }

    #[test]
    fn normal_leader_and_help_keys() {
        let (mut app, script) = loaded();
        press(&mut app, KeyCode::Char('?'));
        assert!(draw_sized(&mut app, 160, 80).contains("ALIASES (command prompt)"));
        press(&mut app, KeyCode::Char('z'));
        assert!(matches!(app.mode, Mode::Help));
        press(&mut app, KeyCode::Esc);

        press(&mut app, KeyCode::Char(' '));
        assert!(draw(&mut app).contains("Space …"));
        press(&mut app, KeyCode::Down);
        assert!(matches!(app.mode, Mode::Normal));
        for (key, tab) in [('h', Tab::Helm), ('F', Tab::Flux), ('l', Tab::Journal), ('x', Tab::Contexts), ('o', Tab::Tooling), ('?', Tab::Cheatsheet), ('z', Tab::Cheatsheet)] {
            press(&mut app, KeyCode::Char(' '));
            press(&mut app, KeyCode::Char(key));
            assert!(app.tab == tab, "{key}");
        }
        for (key, check) in [(':', 0), ('n', 1), ('f', 2)] {
            press(&mut app, KeyCode::Char(' '));
            press(&mut app, KeyCode::Char(key));
            assert!(matches!((check, &app.mode), (0, Mode::Command) | (1, Mode::Namespaces) | (2, Mode::PortForwards)));
            press(&mut app, KeyCode::Esc);
        }
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char('d'));
        assert!(app.tab == Tab::Resources && app.kind().name == "deployments");

        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char('a'));
        assert_eq!((app.namespace.as_deref(), status(&app).as_str()), (None, "namespace: (all namespaces)"));
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.namespace.as_deref(), Some("shop"));
        app.namespace = None;
        app.previous_namespace = None;
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.namespace.as_deref(), Some("default"));

        press(&mut app, KeyCode::Tab);
        assert!(app.tab == Tab::Helm);
        press(&mut app, KeyCode::BackTab);
        press(&mut app, KeyCode::BackTab);
        assert!(app.tab == Tab::Contexts);

        press(&mut app, KeyCode::Char('w'));
        assert_eq!(status(&app), "auto-refresh every 2s on");
        assert!(draw(&mut app).contains("● watch"));
        press(&mut app, KeyCode::Char('w'));
        assert_eq!(status(&app), "auto-refresh every 2s off");
        press(&mut app, KeyCode::Char('J'));
        assert!(!app.show_journal_strip);
        press(&mut app, KeyCode::Char('J'));
        assert!(draw(&mut app).contains("Commands run"));
        assert!(!draw_sized(&mut app, 160, 20).contains("Commands run"));
        press(&mut app, KeyCode::Char('T'));
        assert_eq!(status(&app), "✘ theme: osaka-jade, but could not save the choice: no home directory");

        script.borrow_mut().commands.clear();
        press(&mut app, KeyCode::Char('R'));
        press(&mut app, KeyCode::F(5));
        assert_eq!(script.borrow().commands, ["capture: kubectl config get-contexts", "capture: kubectl config get-contexts"]);
        press(&mut app, KeyCode::Char('P'));
        assert!(draw(&mut app).contains("No port-forwards running"));
        press(&mut app, KeyCode::Esc);

        for tab in Tab::ALL {
            app.tab = tab;
            for key in [KeyCode::Down, KeyCode::Char('j'), KeyCode::Up, KeyCode::Char('k'), KeyCode::PageDown, KeyCode::PageUp] {
                press(&mut app, key);
            }
            for key in [KeyCode::Home, KeyCode::Char('g'), KeyCode::End, KeyCode::Char('G')] {
                press(&mut app, key);
            }
            app.refresh_current(false);
        }
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.status, None);
        press(&mut app, KeyCode::Char('?'));
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.running);
        app.running = true;
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.running);
    }

    #[test]
    fn viewer_receives_output_streams_and_reruns() {
        let (mut app, script) = loaded();
        press(&mut app, KeyCode::Char('d'));
        let generation = app.viewer.as_ref().unwrap().generation;
        let journal = app.viewer.as_ref().unwrap().journal;
        app.handle_msg(Msg::Output { target: Target::Viewer(generation), journal, result: Ok("Name: web-1".into()) });
        app.handle_msg(Msg::Output { target: Target::Viewer(999), journal: 0, result: Err("old".into()) });
        let screen = draw(&mut app);
        assert!(screen.contains("Name: web-1") && screen.contains("close"));
        press(&mut app, KeyCode::Char('j'));
        script.borrow_mut().commands.clear();
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(last(&script), "capture: kubectl describe pods web-1 -n shop");
        press(&mut app, KeyCode::Char('q'));
        assert!(app.viewer.is_none());

        press(&mut app, KeyCode::Char('l'));
        let generation = app.viewer.as_ref().unwrap().generation;
        app.handle_msg(Msg::Line { generation, line: "hello".into() });
        app.handle_msg(Msg::Line { generation: 999, line: "other".into() });
        app.handle_msg(Msg::StreamEnd { generation: 999 });
        app.handle_msg(Msg::StreamEnd { generation });
        let screen = draw(&mut app);
        assert!(screen.contains("hello") && screen.contains("stream ended") && !screen.contains("other"));
        press(&mut app, KeyCode::Char('q'));

        script.borrow_mut().spawn_fails = true;
        press(&mut app, KeyCode::Char('l'));
        assert!(draw(&mut app).contains("failed to start: no such program"));
    }

    #[test]
    fn tick_refreshes_watched_views_and_reaps_port_forwards() {
        let (mut app, script) = loaded();
        app.watch = true;
        for (tab, expected) in [
            (Tab::Resources, "capture: kubectl get pods -n shop -o wide"),
            (Tab::Helm, "capture: helm list --all -o json -n shop"),
            (Tab::Flux, "capture: flux get kustomizations -A"),
            (Tab::Contexts, "capture: kubectl config get-contexts"),
        ] {
            app.tab = tab;
            for pane in [&mut app.resources, &mut app.helm, &mut app.flux, &mut app.contexts] {
                pane.loading = false;
                pane.last_refresh = None;
            }
            script.borrow_mut().commands.clear();
            app.tick();
            app.tick();
            assert_eq!(script.borrow().commands, [expected]);
        }
        app.tab = Tab::Journal;
        script.borrow_mut().commands.clear();
        app.tick();
        assert!(script.borrow().commands.is_empty());

        // An open viewer in watch mode re-runs instead.
        app.tab = Tab::Resources;
        app.resources.set_result(Ok(KubeTable::parse(PODS)));
        press(&mut app, KeyCode::Char('d'));
        let viewer = app.viewer.as_mut().unwrap();
        viewer.set_output(Ok("Name: web-1".into()));
        viewer.watch = Some(Duration::ZERO);
        script.borrow_mut().commands.clear();
        app.tick();
        assert_eq!(script.borrow().commands, ["capture: kubectl describe pods web-1 -n shop"]);
        press(&mut app, KeyCode::Char('q'));

        script.borrow_mut().background = "echo boom >&2; exit 1";
        press(&mut app, KeyCode::Char('f'));
        keys(&mut app, "8080:80");
        press(&mut app, KeyCode::Enter);
        script.borrow_mut().background = "sleep 30";
        press(&mut app, KeyCode::Char('f'));
        keys(&mut app, "9090:80");
        press(&mut app, KeyCode::Enter);
        app.port_forwards[0].child.wait().unwrap();
        app.tick();
        assert_eq!(status(&app), "✘ port-forward localhost:8080 → pods/web-1 exited: boom");
        assert_eq!(app.port_forwards.len(), 1);
    }

    #[test]
    fn port_forward_popup_lists_and_stops_them() {
        let (mut app, _script) = loaded();
        for ports in ["8080:80", "9090:80"] {
            press(&mut app, KeyCode::Char('f'));
            keys(&mut app, ports);
            press(&mut app, KeyCode::Enter);
        }
        press(&mut app, KeyCode::Char('P'));
        let screen = draw(&mut app);
        assert!(screen.contains("localhost:9090 → pods/web-1") && screen.contains("⇄ 2"));
        for key in [KeyCode::Char('j'), KeyCode::Down, KeyCode::Char('k'), KeyCode::Up, KeyCode::Char('z')] {
            press(&mut app, key);
        }
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(status(&app), "stopped localhost:8080 → pods/web-1");
        press(&mut app, KeyCode::Delete);
        assert!(app.port_forwards.is_empty());
        press(&mut app, KeyCode::Char('x'));
        for key in [KeyCode::Char('q'), KeyCode::Char('P'), KeyCode::Esc] {
            app.mode = Mode::PortForwards;
            press(&mut app, key);
            assert!(matches!(app.mode, Mode::Normal));
        }
    }

    #[test]
    fn panes_show_errors_empty_lists_and_loading() {
        let (mut app, _script) = loaded();
        app.resources.reset();
        app.resources.set_result(Err("error: the server doesn't have a resource type".into()));
        assert!(draw(&mut app).contains("doesn't have a resource type"));
        app.resources.reset();
        app.resources.set_result(Ok(KubeTable::default()));
        assert!(draw(&mut app).contains("No resources found."));
        app.resources.loading = true;
        app.resources.filter = "zz".into();
        assert!(draw(&mut app).contains("loading…"));

        press(&mut app, KeyCode::Char('1'));
        app.on_output(Target::Contexts, Ok(CONTEXTS.into()));
        assert!(draw(&mut app).contains("staging"));
        app.status = Some(("all good".into(), false));
        assert!(draw(&mut app).contains("all good"));

        // Every kind's hints, and a kind list with cluster-wide kinds dimmed.
        for index in 0..KINDS.len() {
            app.tab = Tab::Resources;
            app.kind_index = index;
            draw(&mut app);
        }
        for tab in Tab::ALL {
            app.tab = tab;
            assert!(draw(&mut app).contains(tab.title()));
        }
    }

    #[test]
    fn events_are_sorted_and_rerun_needs_a_viewer() {
        let (mut app, script) = loaded();
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char('e'));
        assert_eq!(last(&script), "capture: kubectl get events -n shop --sort-by=.lastTimestamp");
        script.borrow_mut().commands.clear();
        app.rerun_viewer(false);
        assert!(script.borrow().commands.is_empty());
    }

    #[test]
    fn themes_are_saved_and_theme_problems_reported() {
        let dir = helpers::test_dir("app-themes");
        fs::create_dir_all(dir.join("themes")).unwrap();
        fs::write(dir.join("themes").join("bad.toml"), "accent = \"nope\"\n").unwrap();
        let script = Rc::new(RefCell::new(Script::default()));
        let mut app = App::with_host(Box::new(FakeHost(script)), Some(dir.clone()), Some(dir.join("journal")), None);
        assert!(status(&app).starts_with("✘ theme: "));
        press(&mut app, KeyCode::Char('T'));
        assert!(status(&app).starts_with("theme: bad  (saved in"), "{}", status(&app));
        drop(app);
        fs::remove_dir_all(&dir).unwrap();
    }
}
