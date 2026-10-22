use std::io;
use std::process::Child;
use std::sync::mpsc::Sender;
use std::time::Duration;

use ratatui::crossterm::event::Event;

use crate::data_structures::{Msg, Target};

/// Everything the app needs from the outside world: starting processes, reading
/// the keyboard and handing over the real terminal. Tests substitute a scripted
/// host so no real kubectl/helm/flux command is ever run against a cluster.
pub trait Host {
    /// Runs a command in the background; the result arrives as `Msg::Output`.
    fn capture(&mut self, argv: Vec<String>, target: Target, journal: u64, tx: Sender<Msg>);
    /// Runs a quick local command and waits for it (kubeconfig reads and splits).
    fn run(&mut self, argv: &[String]) -> Result<String, String>;
    /// Starts a long-running command whose output lines arrive as `Msg::Line`.
    fn stream(&mut self, argv: &[String], generation: u64, tx: &Sender<Msg>) -> io::Result<Child>;
    /// Starts a detached helper process (port-forward).
    fn background(&mut self, argv: &[String]) -> io::Result<Child>;
    /// Waits up to `timeout` for the next terminal event.
    fn next_event(&mut self, timeout: Duration) -> io::Result<Option<Event>>;
    /// Runs a command on the real terminal (exec -it, edit, `!cmd`).
    fn interactive(&mut self, argv: &[String], pause: bool) -> io::Result<Result<(), String>>;
}

/// The real host: processes via `std::process`, keys via crossterm, the real tty.
/// Only this terminal glue is left out of test builds; it cannot run without a tty.
#[cfg(not(test))]
pub struct SystemHost;

#[cfg(not(test))]
impl Host for SystemHost {
    fn capture(&mut self, argv: Vec<String>, target: Target, journal: u64, tx: Sender<Msg>) {
        crate::helpers::spawn_capture(argv, target, journal, tx);
    }

    fn run(&mut self, argv: &[String]) -> Result<String, String> {
        crate::helpers::run_capture(argv)
    }

    fn stream(&mut self, argv: &[String], generation: u64, tx: &Sender<Msg>) -> io::Result<Child> {
        crate::helpers::spawn_stream(argv, generation, tx)
    }

    fn background(&mut self, argv: &[String]) -> io::Result<Child> {
        crate::helpers::spawn_background(argv)
    }

    fn next_event(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        use ratatui::crossterm::event;
        if event::poll(timeout)? { event::read().map(Some) } else { Ok(None) }
    }

    /// Leaves the TUI, runs the command on the real terminal, then comes back.
    /// With `pause`, waits for Enter so the output can be read; otherwise only pauses on failure.
    fn interactive(&mut self, argv: &[String], pause: bool) -> io::Result<Result<(), String>> {
        use std::io::Write;

        use ratatui::crossterm::execute;
        use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};

        disable_raw_mode()?;
        execute!(io::stdout(), LeaveAlternateScreen)?;
        println!("\x1b[2m$ {}\x1b[0m", crate::helpers::shell_join(argv));

        let outcome = crate::helpers::exit_outcome(argv, crate::helpers::command(argv).status());
        if let Err(e) = &outcome {
            println!("\n[{e}]");
        }
        if pause || outcome.is_err() {
            print!("\nPress Enter to return to kubewatcher...");
            io::stdout().flush()?;
            let mut line = String::new();
            io::stdin().read_line(&mut line)?;
        }

        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(outcome)
    }
}
