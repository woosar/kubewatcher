use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table, TableState};

use crate::data_structures::Theme;
use crate::helpers;

const MAX_ENTRIES: usize = 5_000;

/// How a command was run.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Run to completion, output captured.
    Capture,
    /// Long-running, output streamed (logs -f, get -w, flux reconcile).
    Stream,
    /// Handed the real terminal (exec -it, edit).
    Interactive,
    /// Detached helper process (port-forward).
    Background,
}

struct Outcome {
    ok: bool,
    elapsed: Duration,
    summary: String,
}

struct JournalEntry {
    id: u64,
    at: DateTime<Local>,
    started: Instant,
    command: String,
    origin: String,
    mode: RunMode,
    change: bool,
    auto: bool,
    repeats: u32,
    outcome: Option<Outcome>,
}

impl JournalEntry {
    fn kind_label(&self) -> &'static str {
        match (self.change, self.mode) {
            (true, _) => "CHANGE",
            (false, RunMode::Capture) => "read",
            (false, RunMode::Stream) => "stream",
            (false, RunMode::Interactive) => "shell",
            (false, RunMode::Background) => "bg",
        }
    }

    fn status(&self, theme: &Theme) -> (Span<'static>, String) {
        match &self.outcome {
            None => (Span::styled("…", theme.fg(theme.warning)), "running".into()),
            Some(o) => {
                let icon = if o.ok {
                    Span::styled("✔", theme.fg(theme.success))
                } else {
                    Span::styled("✘", theme.fg(theme.danger).bold())
                };
                (icon, format!("{:.2}s", o.elapsed.as_secs_f64()))
            }
        }
    }
}

/// Log of every command kubewatcher runs on your behalf: the bridge between
/// "I pressed a key" and "this is the kubectl/helm/flux command that did it".
/// Non-automatic entries are also appended to one log file per day.
pub struct Journal {
    entries: Vec<JournalEntry>,
    next_id: u64,
    /// `~/.local/state/kubewatcher/journal/`, if it could be created.
    dir: Option<PathBuf>,
    /// Today's open log file and the day it belongs to.
    file: Option<(String, File)>,
    pub filter: String,
    pub changes_only: bool,
    pub hide_auto: bool,
    state: TableState,
    follow: bool,
}

impl Journal {
    /// Logs go to `$XDG_STATE_HOME/kubewatcher/journal/YYYY-MM-DD.log`
    /// (default `~/.local/state/kubewatcher/journal/`), one file per day.
    pub fn open() -> Self {
        let dir = helpers::xdg_dir("XDG_STATE_HOME", &[".local", "state"])
            .map(|dir| dir.join("kubewatcher").join("journal"))
            .filter(|dir| fs::create_dir_all(dir).is_ok());
        Self {
            entries: Vec::new(),
            next_id: 0,
            dir,
            file: None,
            filter: String::new(),
            changes_only: false,
            hide_auto: false,
            state: TableState::default(),
            follow: true,
        }
    }

    /// Appends to the file for `day`, switching files when the day changes.
    fn append(&mut self, day: String, text: &str) {
        let Some(dir) = &self.dir else { return };
        if self.file.as_ref().is_none_or(|(open_day, _)| *open_day != day) {
            let path = dir.join(format!("{day}.log"));
            self.file = OpenOptions::new().create(true).append(true).open(path).ok().map(|f| (day, f));
        }
        if let Some((_, file)) = self.file.as_mut() {
            let _ = writeln!(file, "{text}");
        }
    }

    /// Records a command that is about to run and returns its journal id.
    /// Consecutive identical automatic commands (watch polling) collapse into one entry.
    pub fn record(&mut self, argv: &[String], origin: &str, mode: RunMode, auto: bool) -> u64 {
        let command = helpers::shell_join(argv);
        if auto
            && let Some(last) = self.entries.iter_mut().rev().find(|e| e.auto && e.command == command)
            && last.outcome.is_some()
        {
            last.repeats += 1;
            last.at = Local::now();
            last.started = Instant::now();
            last.outcome = None;
            return last.id;
        }
        self.next_id += 1;
        self.entries.push(JournalEntry {
            id: self.next_id,
            at: Local::now(),
            started: Instant::now(),
            change: Self::is_change(&command),
            command,
            origin: origin.to_string(),
            mode,
            auto,
            repeats: 1,
            outcome: None,
        });
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
        self.next_id
    }

    /// Marks a command as finished; the first line of its output/error becomes the summary.
    pub fn finish(&mut self, id: u64, ok: bool, output: &str) {
        let Some(entry) = self.entries.iter_mut().rev().find(|e| e.id == id) else {
            return;
        };
        if entry.outcome.is_some() {
            return;
        }
        let summary: String = output
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .chars()
            .take(160)
            .collect();
        let outcome = Outcome { ok, elapsed: entry.started.elapsed(), summary };

        // Logged under the day the command started.
        let line = (!entry.auto).then(|| {
            let text = format!(
                "{}  {:<4} {:>7.2}s  {:<6}  {}\n    $ {}\n    → {}",
                entry.at.format("%Y-%m-%d %H:%M:%S"),
                if outcome.ok { "OK" } else { "FAIL" },
                outcome.elapsed.as_secs_f64(),
                entry.kind_label(),
                entry.origin,
                entry.command,
                if outcome.summary.is_empty() { "(no output)" } else { &outcome.summary },
            );
            (entry.at.format("%Y-%m-%d").to_string(), text)
        });
        entry.outcome = Some(outcome);
        if let Some((day, text)) = line {
            self.append(day, &text);
        }
    }

    /// Whether a command modifies the cluster or your kubeconfig.
    fn is_change(command: &str) -> bool {
        let words: Vec<&str> = command.split_whitespace().collect();
        let verb = words.get(1).copied().unwrap_or("");
        let has = |w: &str| words.contains(&w);
        let dry_run = command.contains("--dry-run") || has("--export") || has("template");
        let changing = match words.first().copied() {
            Some("kubectl") => {
                [
                    "apply", "create", "delete", "edit", "patch", "replace", "scale", "label", "annotate", "set",
                    "cordon", "uncordon", "drain", "taint", "autoscale", "expose", "run",
                ]
                .contains(&verb)
                    || (verb == "rollout" && ["restart", "undo", "pause", "resume"].iter().any(|w| has(w)))
                    || (verb == "config" && ["use-context", "set-context", "delete-context", "rename-context"].iter().any(|w| has(w)))
            }
            Some("helm") => {
                ["install", "upgrade", "uninstall", "rollback"].contains(&verb)
                    || (verb == "repo" && (has("add") || has("remove")))
            }
            Some("flux") => ["suspend", "resume", "reconcile", "delete", "bootstrap", "install", "uninstall", "create"].contains(&verb),
            _ => false,
        };
        changing && !dry_run
    }

    fn visible(&self) -> Vec<usize> {
        (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                (!self.changes_only || e.change)
                    && (!self.hide_auto || !e.auto)
                    && helpers::matches_filter([e.command.as_str(), e.origin.as_str()], &self.filter)
            })
            .collect()
    }

    pub fn step(&mut self, delta: isize) {
        let len = self.visible().len();
        let selected = helpers::step_selection(self.state.selected(), len, delta);
        self.state.select(selected);
        self.follow = selected.is_none_or(|s| s + 1 >= len);
    }

    pub fn clamp(&mut self) {
        let len = self.visible().len();
        self.state.select(helpers::clamp_selection(self.state.selected(), len));
    }

    pub fn selected_command(&self) -> Option<String> {
        let visible = self.visible();
        self.state
            .selected()
            .and_then(|i| visible.get(i))
            .map(|&i| self.entries[i].command.clone())
    }

    /// Full journal view (its own tab).
    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let visible = self.visible();
        if self.follow {
            self.state.select(visible.len().checked_sub(1));
        }
        let rows = visible.iter().map(|&i| {
            let e = &self.entries[i];
            let (icon, duration) = e.status(theme);
            let time = if e.repeats > 1 {
                format!("{} ×{}", e.at.format("%H:%M:%S"), e.repeats)
            } else {
                e.at.format("%H:%M:%S").to_string()
            };
            let command_style = if e.change {
                theme.fg(theme.warning).add_modifier(Modifier::BOLD)
            } else if e.auto {
                theme.fg(theme.muted)
            } else {
                Style::new()
            };
            let summary_style = match &e.outcome {
                Some(o) if !o.ok => theme.fg(theme.danger),
                _ => theme.fg(theme.muted),
            };
            Row::new([
                Cell::from(time),
                Cell::from(icon),
                Cell::from(duration),
                Cell::from(e.kind_label()).style(command_style),
                Cell::from(e.origin.as_str()).style(theme.fg(theme.accent)),
                Cell::from(Line::from(vec![
                    Span::styled(format!("$ {}", e.command), command_style),
                    Span::styled(
                        e.outcome.as_ref().map(|o| format!("   → {}", o.summary)).unwrap_or_default(),
                        summary_style,
                    ),
                ])),
            ])
        });

        let mut flags = Vec::new();
        if self.changes_only {
            flags.push("changes only");
        }
        if self.hide_auto {
            flags.push("auto hidden");
        }
        let mut title = format!(" Journal ({}) ", visible.len());
        if !flags.is_empty() {
            title.push_str(&format!("[{}] ", flags.join(", ")));
        }
        if !self.filter.is_empty() {
            title.push_str(&format!("/{} ", self.filter));
        }
        let file = self.dir.as_ref().map_or("not saved to disk".to_string(), |d| format!("saved to {}/<day>.log", d.display()));
        let block = theme
            .block()
            .title(title)
            .title_bottom(Line::from(format!(" {file} ")).dim())
            .title_bottom(Line::from(" Enter → prompt · c changes only · a hide auto ").right_aligned().dim());
        let header = Row::new(["TIME", "", "TOOK", "KIND", "TRIGGERED BY", "COMMAND"])
            .style(theme.header());
        let table = Table::new(
            rows,
            [
                Constraint::Length(13),
                Constraint::Length(1),
                Constraint::Length(8),
                Constraint::Length(6),
                Constraint::Length(34),
                Constraint::Fill(1),
            ],
        )
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(theme.selected())
        .highlight_symbol("▶ ");
        frame.render_stateful_widget(table, area, &mut self.state);
    }

    /// Compact "last few commands" strip shown under every view.
    pub fn render_strip(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let height = area.height.saturating_sub(2) as usize;
        let start = self.entries.len().saturating_sub(height);
        let lines: Vec<Line> = self.entries[start..]
            .iter()
            .map(|e| {
                let (icon, duration) = e.status(theme);
                let style = if e.change {
                    theme.fg(theme.warning).bold()
                } else if e.auto {
                    theme.fg(theme.muted)
                } else {
                    Style::new()
                };
                let quiet = theme.fg(theme.muted);
                let mut spans = vec![
                    Span::styled(format!("{} ", e.at.format("%H:%M:%S")), quiet),
                    icon,
                    Span::styled(format!(" {duration:>7} "), quiet),
                ];
                if e.repeats > 1 {
                    spans.push(Span::styled(format!("×{} ", e.repeats), quiet));
                }
                spans.push(Span::styled(format!("$ {}", e.command), style));
                spans.push(Span::styled(format!("   ← {}", e.origin), theme.fg(theme.accent)));
                Line::from(spans)
            })
            .collect();
        let block = Block::bordered()
            .border_style(theme.fg(theme.muted))
            .title(Line::from(" Commands run · J hide · 5 full journal ").style(theme.fg(theme.muted)));
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal() -> Journal {
        Journal {
            entries: Vec::new(),
            next_id: 0,
            dir: None,
            file: None,
            filter: String::new(),
            changes_only: false,
            hide_auto: false,
            state: TableState::default(),
            follow: true,
        }
    }

    #[test]
    fn classifies_changes() {
        assert!(Journal::is_change("flux suspend kustomization apps -n flux-system"));
        assert!(Journal::is_change("kubectl rollout restart deployments/web -n shop"));
        assert!(Journal::is_change("kubectl config use-context prod"));
        assert!(Journal::is_change("helm rollback web 2 -n shop"));
        assert!(!Journal::is_change("kubectl get pods -n shop -o wide"));
        assert!(!Journal::is_change("kubectl rollout status -w deployments/web"));
        assert!(!Journal::is_change("kubectl create deployment web --image=nginx --dry-run=client -o yaml"));
        assert!(!Journal::is_change("flux create source helm bitnami --url=https://x --export"));
        assert!(!Journal::is_change("flux get kustomizations -A"));
    }

    #[test]
    fn collapses_repeated_auto_commands() {
        let mut j = journal();
        let argv = helpers::argv(&["kubectl", "get", "pods"]);
        let first = j.record(&argv, "watch", RunMode::Capture, true);
        j.finish(first, true, "NAME ...");
        let second = j.record(&argv, "watch", RunMode::Capture, true);
        assert_eq!(first, second);
        assert_eq!(j.entries.len(), 1);
        assert_eq!(j.entries[0].repeats, 2);

        // A manual run of the same command is always its own entry.
        j.record(&argv, "you", RunMode::Capture, false);
        assert_eq!(j.entries.len(), 2);
    }

    #[test]
    fn finish_records_summary_once() {
        let mut j = journal();
        let id = j.record(&helpers::argv(&["kubectl", "delete", "pod", "x"]), "Resources ▸ delete", RunMode::Capture, false);
        j.finish(id, false, "\nError from server (NotFound): pods \"x\" not found\n");
        j.finish(id, true, "ignored");
        let outcome = j.entries[0].outcome.as_ref().unwrap();
        assert!(!outcome.ok);
        assert_eq!(outcome.summary, "Error from server (NotFound): pods \"x\" not found");
        assert!(j.entries[0].change);
    }
}
