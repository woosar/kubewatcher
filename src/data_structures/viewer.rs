use std::process::{Child, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::data_structures::Theme;
use crate::helpers;

/// Keep streaming output bounded so `logs -f` can run for hours.
const MAX_LINES: usize = 20_000;
pub const DEFAULT_WATCH: Duration = Duration::from_secs(2);
const EXIT_WAIT_TRIES: usize = 20;
const EXIT_WAIT_STEP: Duration = Duration::from_millis(10);

pub enum ViewerAction {
    None,
    Close,
    Rerun,
}

/// Full-screen scrollable output of one command: a one-shot capture (optionally
/// re-run on an interval, like `watch`) or a live stream (`logs -f`, `get -w`).
pub struct Viewer {
    pub title: String,
    pub argv: Vec<String>,
    pub generation: u64,
    /// Journal entry of the run currently shown.
    pub journal: u64,
    pub watch: Option<Duration>,
    loading: bool,
    lines: Vec<String>,
    is_error: bool,
    scroll: usize,
    hscroll: u16,
    follow: bool,
    height: usize,
    stream: Option<Child>,
    stream_ended: bool,
    last_run: Instant,
}

impl Viewer {
    pub fn new(title: String, argv: Vec<String>, generation: u64) -> Self {
        Self {
            title,
            argv,
            generation,
            journal: 0,
            watch: None,
            loading: false,
            lines: Vec::new(),
            is_error: false,
            scroll: 0,
            hscroll: 0,
            follow: false,
            height: 0,
            stream: None,
            stream_ended: false,
            last_run: Instant::now(),
        }
    }

    pub fn attach_stream(&mut self, child: Child) {
        self.stream = Some(child);
        self.follow = true;
    }

    pub fn mark_running(&mut self) {
        self.loading = true;
        self.last_run = Instant::now();
    }

    pub fn watch_due(&self) -> bool {
        matches!(self.watch, Some(interval) if !self.loading && self.stream.is_none() && self.last_run.elapsed() >= interval)
    }

    pub fn set_output(&mut self, result: Result<String, String>) {
        self.loading = false;
        self.is_error = result.is_err();
        let text = result.unwrap_or_else(|e| e);
        self.lines = text.lines().map(helpers::sanitize_line).collect();
        if self.lines.is_empty() {
            self.lines.push("(no output)".into());
        }
        // Watch mode re-renders in place, so keep the scroll position where possible.
        self.scroll = self.scroll.min(self.max_scroll());
    }

    pub fn push_line(&mut self, line: &str) {
        self.lines.push(helpers::sanitize_line(line));
        if self.lines.len() > MAX_LINES {
            let excess = self.lines.len() - MAX_LINES;
            self.lines.drain(..excess);
            self.scroll = self.scroll.saturating_sub(excess);
        }
    }

    /// Its output has ended; how the command itself ended is what the journal should record.
    pub fn end_stream(&mut self) -> Result<(), String> {
        self.stream_ended = true;
        let outcome = self.exit_status().map_or(Ok(()), |status| {
            // `flux diff` exits 1 to say "there are differences", which is an answer, not a failure.
            let found_differences = self.argv.starts_with(&["flux".to_string(), "diff".to_string()]) && status.code() == Some(1);
            if found_differences { Ok(()) } else { helpers::exit_outcome(&self.argv, Ok(status)) }
        });
        match &outcome {
            Ok(()) => self.push_line("── stream ended ──"),
            Err(e) => self.push_line(&format!("── failed: {e} ──")),
        }
        outcome
    }

    /// Output closes just before the process is gone, so it gets a moment; anything that keeps
    /// running after closing its output isn't waited for, as this runs on the UI thread.
    fn exit_status(&mut self) -> Option<ExitStatus> {
        let child = self.stream.as_mut()?;
        for _ in 0..EXIT_WAIT_TRIES {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => thread::sleep(EXIT_WAIT_STEP),
                Err(_) => return None,
            }
        }
        None
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ViewerAction {
        let page = self.height.max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return ViewerAction::Close,
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(-1),
            KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('d') => self.scroll_by(page as isize),
            KeyCode::PageUp | KeyCode::Char('u') => self.scroll_by(-(page as isize)),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_by(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll = self.max_scroll();
                self.follow = self.stream.is_some();
            }
            KeyCode::Left | KeyCode::Char('h') => self.hscroll = self.hscroll.saturating_sub(8),
            KeyCode::Right | KeyCode::Char('l') => self.hscroll = self.hscroll.saturating_add(8),
            KeyCode::Char('f') if self.stream.is_some() => {
                self.follow = !self.follow;
                if self.follow {
                    self.scroll = self.max_scroll();
                }
            }
            KeyCode::Char('w') if self.stream.is_none() => {
                self.watch = match self.watch {
                    Some(_) => None,
                    None => Some(DEFAULT_WATCH),
                };
            }
            KeyCode::Char('r') if self.stream.is_none() => return ViewerAction::Rerun,
            _ => {}
        }
        ViewerAction::None
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = (self.scroll as isize + delta).clamp(0, self.max_scroll() as isize) as usize;
        if delta < 0 {
            self.follow = false;
        }
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.height.max(1))
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.height = area.height.saturating_sub(2) as usize;
        if self.follow {
            self.scroll = self.max_scroll();
        }

        let mut badges = Vec::new();
        if let Some(interval) = self.watch {
            badges.push(Span::styled(format!(" watch {}s ", interval.as_secs()), theme.badge(theme.success)));
        }
        if self.stream.is_some() && !self.stream_ended {
            let label = if self.follow { " live · follow " } else { " live · paused " };
            badges.push(Span::styled(label, theme.badge(theme.secondary)));
        }
        if self.loading {
            badges.push(Span::styled(" running… ", theme.badge(theme.warning)));
        }
        let position = format!(" {}/{} ", (self.scroll + self.height).min(self.lines.len()), self.lines.len());

        let mut title = vec![Span::raw(" "), Span::styled(self.title.clone(), Style::new().bold()), Span::raw(" ")];
        title.extend(badges);

        let block = theme
            .block()
            .border_style(theme.fg(theme.secondary))
            .title(Line::from(title))
            .title(Line::from(position).right_aligned())
            .title_bottom(Line::from(format!(" $ {} ", helpers::shell_join(&self.argv))).dim())
            .title_bottom(
                Line::from(" q close · j/k ↑↓ · space/u page · g/G · h/l ←→ · w watch · r rerun · f follow ")
                    .right_aligned()
                    .dim(),
            );

        let style = if self.is_error { theme.fg(theme.danger) } else { Style::new() };
        let visible: Vec<Line> = self
            .lines
            .iter()
            .skip(self.scroll)
            .take(self.height)
            .map(|l| Line::styled(l.as_str(), style))
            .collect();

        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(visible).style(theme.base()).block(block).scroll((0, self.hscroll)), area);
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        if let Some(child) = self.stream.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyModifiers;

    use super::*;

    fn press(viewer: &mut Viewer, code: KeyCode) -> ViewerAction {
        viewer.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn draw(viewer: &mut Viewer) -> String {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(100, 8)).unwrap();
        terminal.draw(|frame| viewer.render(frame, frame.area(), &Theme::default())).unwrap();
        terminal.backend().to_string()
    }

    fn sleeper() -> Child {
        Command::new("sh").args(["-c", "sleep 30"]).spawn().unwrap()
    }

    #[test]
    fn captured_output_scrolls_and_watches() {
        let mut viewer = Viewer::new("describe pod/web".into(), helpers::argv(&["kubectl", "describe", "pod", "web"]), 1);
        viewer.mark_running();
        assert!(draw(&mut viewer).contains("running…"));
        viewer.set_output(Ok((1..=30).map(|i| format!("line {i}\n")).collect()));
        assert!(draw(&mut viewer).contains("line 1 "));

        assert!(matches!(press(&mut viewer, KeyCode::Char('j')), ViewerAction::None));
        press(&mut viewer, KeyCode::Char(' '));
        press(&mut viewer, KeyCode::Char('k'));
        press(&mut viewer, KeyCode::Char('u'));
        press(&mut viewer, KeyCode::Char('G'));
        assert!(draw(&mut viewer).contains("30/30"));
        press(&mut viewer, KeyCode::Char('g'));
        press(&mut viewer, KeyCode::Char('l'));
        press(&mut viewer, KeyCode::Char('h'));
        press(&mut viewer, KeyCode::Char('f'));
        press(&mut viewer, KeyCode::Char('x'));

        assert!(!viewer.watch_due());
        press(&mut viewer, KeyCode::Char('w'));
        assert!(draw(&mut viewer).contains("watch 2s"));
        viewer.watch = Some(Duration::ZERO);
        assert!(viewer.watch_due());
        press(&mut viewer, KeyCode::Char('w'));
        assert!(viewer.watch.is_none());
        assert!(matches!(press(&mut viewer, KeyCode::Char('r')), ViewerAction::Rerun));
        assert!(matches!(press(&mut viewer, KeyCode::Esc), ViewerAction::Close));
    }

    #[test]
    fn errors_and_empty_output_are_shown() {
        let mut viewer = Viewer::new("yaml".into(), Vec::new(), 1);
        viewer.set_output(Ok(String::new()));
        assert!(draw(&mut viewer).contains("(no output)"));
        viewer.set_output(Err("Error from server (NotFound)".into()));
        assert!(viewer.is_error && draw(&mut viewer).contains("NotFound"));
    }

    #[test]
    fn streams_follow_pause_and_stay_bounded() {
        let mut viewer = Viewer::new("logs".into(), helpers::argv(&["kubectl", "logs", "-f", "web"]), 2);
        viewer.attach_stream(sleeper());
        for i in 0..=MAX_LINES {
            viewer.push_line(&format!("log {i}"));
        }
        assert_eq!(viewer.lines.len(), MAX_LINES);
        assert!(draw(&mut viewer).contains("live · follow"));
        press(&mut viewer, KeyCode::Char('f'));
        assert!(draw(&mut viewer).contains("live · paused"));
        press(&mut viewer, KeyCode::Char('f'));
        press(&mut viewer, KeyCode::Up);
        assert!(!viewer.follow);
        press(&mut viewer, KeyCode::End);
        assert!(viewer.follow);
        assert!(matches!(press(&mut viewer, KeyCode::Char('r')), ViewerAction::None));
        press(&mut viewer, KeyCode::Char('w'));
        assert!(viewer.watch.is_none());
        assert!(viewer.end_stream().is_ok());
        assert_eq!(viewer.lines.last().map(String::as_str), Some("── stream ended ──"));
        // Dropping the viewer kills the still-running process.
    }

    #[test]
    fn how_a_stream_ended_comes_from_its_exit_status() {
        let ended = |argv: &[&str], script: &str| {
            let mut viewer = Viewer::new("t".into(), helpers::argv(argv), 1);
            viewer.attach_stream(helpers::spawn_background(&helpers::shell(script)).unwrap());
            (viewer.end_stream(), viewer.lines.last().cloned().unwrap_or_default())
        };
        assert_eq!(ended(&["flux", "reconcile"], "exit 0").0, Ok(()));

        let (outcome, line) = ended(&["flux", "reconcile"], "exit 3");
        assert!(outcome.is_err() && line.starts_with("── failed:"), "{line}");

        // Differences found is an answer; anything else from flux diff is still a failure.
        assert_eq!(ended(&["flux", "diff", "kustomization", "a"], "exit 1").0, Ok(()));
        assert!(ended(&["flux", "diff", "kustomization", "a"], "exit 2").0.is_err());
    }
}
