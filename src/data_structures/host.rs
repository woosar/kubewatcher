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
    /// Like `interactive`, but also hands back what the command printed (ssh + sudo cat):
    /// password prompts reach the terminal, everything from `hide_from` on is only captured.
    fn interactive_capture(&mut self, argv: &[String], hide_from: &str) -> io::Result<Result<String, String>>;
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

    fn interactive_capture(&mut self, argv: &[String], hide_from: &str) -> io::Result<Result<String, String>> {
        use std::io::{Read, Write};
        use std::process::Stdio;

        use ratatui::crossterm::execute;
        use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};

        disable_raw_mode()?;
        execute!(io::stdout(), LeaveAlternateScreen)?;
        println!("\x1b[2m$ {}\x1b[0m", crate::helpers::shell_join(argv));

        let mut captured = Vec::new();
        let outcome = match crate::helpers::command(argv).stdout(Stdio::piped()).spawn() {
            Ok(mut child) => {
                // Echo until the secret part starts, so prompts stay visible but keys never hit the screen.
                let mut stdout = child.stdout.take().expect("stdout is piped");
                let mut shown = 0;
                let mut buf = [0u8; 4096];
                loop {
                    let n = match stdout.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    captured.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&captured);
                    let visible = text.find(hide_from).unwrap_or(text.len());
                    if visible > shown {
                        print!("{}", &text[shown..visible]);
                        io::stdout().flush()?;
                        shown = visible;
                    }
                }
                crate::helpers::exit_outcome(argv, child.wait())
            }
            Err(e) => crate::helpers::exit_outcome(argv, Err(e)),
        };
        let found = String::from_utf8_lossy(&captured).contains(hide_from);
        if found {
            println!("\n[read {} bytes, contents not shown]", captured.len());
        }
        if let Err(e) = &outcome {
            println!("\n[{e}]");
        }
        if outcome.is_err() || !found {
            print!("\nPress Enter to return to kubewatcher...");
            io::stdout().flush()?;
            let mut line = String::new();
            io::stdin().read_line(&mut line)?;
        }

        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(outcome.map(|()| String::from_utf8_lossy(&captured).into_owned()))
    }
}
