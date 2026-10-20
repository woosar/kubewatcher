use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::data_structures::{Msg, Target};

/// Builds an owned argv from string slices.
pub fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| p.to_string()).collect()
}

/// Wraps a free-form command line so it runs through `sh -c`.
pub fn shell(command: &str) -> Vec<String> {
    argv(&["sh", "-c", command])
}

/// Renders an argv as a copy-pasteable shell command.
pub fn shell_join(argv: &[String]) -> String {
    // A `sh -c "<cmd>"` wrapper is an implementation detail; show the inner command.
    if argv.len() == 3 && argv[0] == "sh" && argv[1] == "-c" {
        return argv[2].clone();
    }
    argv.iter()
        .map(|arg| {
            let needs_quotes = arg.is_empty()
                || arg
                    .chars()
                    .any(|c| c.is_whitespace() || "\"'$`\\|&;<>(){}*?!#".contains(c));
            if needs_quotes {
                format!("'{}'", arg.replace('\'', r"'\''"))
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Runs a command to completion; Ok(stdout) on success, Err(stderr) otherwise.
pub fn run_capture(argv: &[String]) -> Result<String, String> {
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to run `{}`: {e}", argv[0]))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Err(match (stderr.is_empty(), stdout.is_empty()) {
            (false, _) => stderr,
            (true, false) => stdout,
            (true, true) => format!("exited with {}", output.status),
        })
    }
}

/// Runs a command on a background thread and reports the result as `Msg::Output`.
/// `journal` is the journal entry id the result is recorded against.
pub fn spawn_capture(argv: Vec<String>, target: Target, journal: u64, tx: Sender<Msg>) {
    thread::spawn(move || {
        let result = run_capture(&argv);
        let _ = tx.send(Msg::Output { target, journal, result });
    });
}

/// Spawns a long-running command (logs -f, get -w, ...) whose stdout and stderr
/// lines are forwarded as `Msg::Line`. The caller owns the child and must kill it.
pub fn spawn_stream(argv: &[String], generation: u64, tx: &Sender<Msg>) -> io::Result<Child> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(stdout) = child.stdout.take() {
        forward_lines(stdout, generation, tx.clone(), true);
    }
    if let Some(stderr) = child.stderr.take() {
        forward_lines(stderr, generation, tx.clone(), false);
    }
    Ok(child)
}

fn forward_lines(reader: impl Read + Send + 'static, generation: u64, tx: Sender<Msg>, signal_end: bool) {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf).trim_end_matches(['\n', '\r']).to_string();
                    if tx.send(Msg::Line { generation, line }).is_err() {
                        return;
                    }
                }
            }
        }
        if signal_end {
            let _ = tx.send(Msg::StreamEnd { generation });
        }
    });
}

/// Spawns a detached helper process (e.g. port-forward); stderr is kept for diagnostics.
pub fn spawn_background(argv: &[String]) -> io::Result<Child> {
    Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
}

/// Leaves the TUI, runs an interactive command on the real terminal, then comes back.
/// With `pause`, waits for Enter so the output can be read; otherwise only pauses on failure.
/// The inner result says whether the command itself succeeded.
pub fn run_interactive(terminal: &mut DefaultTerminal, argv: &[String], pause: bool) -> io::Result<Result<(), String>> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    println!("\x1b[2m$ {}\x1b[0m", shell_join(argv));

    let status = Command::new(&argv[0]).args(&argv[1..]).status();
    let outcome = match &status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(status.to_string()),
        Err(e) => Err(format!("failed to run `{}`: {e}", argv[0])),
    };
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
    terminal.clear()?;
    Ok(outcome)
}

/// Strips ANSI escape sequences and control characters so raw command output
/// cannot corrupt the terminal; tabs become spaces.
pub fn sanitize_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    // CSI: parameters until a final byte in '@'..='~'
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Moves a list selection by `delta`, clamped to `0..len`.
pub fn step_selection(current: Option<usize>, len: usize, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let current = current.unwrap_or(0) as isize;
    Some((current + delta).clamp(0, len as isize - 1) as usize)
}

/// Keeps a selection valid after the underlying list changed size.
pub fn clamp_selection(current: Option<usize>, len: usize) -> Option<usize> {
    match (current, len) {
        (_, 0) => None,
        (None, _) => Some(0),
        (Some(i), len) => Some(i.min(len - 1)),
    }
}

/// Applies a single-line text-editing key to `buf`. Returns false if the key is not an edit.
pub fn edit_input(buf: &mut String, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('u') if ctrl => buf.clear(),
        KeyCode::Char('w') if ctrl => {
            let trimmed = buf.trim_end().len();
            buf.truncate(trimmed);
            let cut = buf.rfind(' ').map_or(0, |i| i + 1);
            buf.truncate(cut);
        }
        KeyCode::Char(c) if !ctrl => buf.push(c),
        KeyCode::Backspace => {
            buf.pop();
        }
        _ => return false,
    }
    true
}

/// Case-insensitive "contains" over any of the given cells.
pub fn matches_filter<'a>(cells: impl IntoIterator<Item = &'a str>, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let filter = filter.to_lowercase();
    cells.into_iter().any(|cell| cell.to_lowercase().contains(&filter))
}

/// Short aliases accepted in the command prompt; expanded before running.
pub const ALIASES: &[(&str, &str)] = &[
    ("k", "kubectl"),
    ("kg", "kubectl get"),
    ("kgp", "kubectl get pods"),
    ("kgs", "kubectl get svc"),
    ("kgd", "kubectl get deploy"),
    ("kd", "kubectl describe"),
    ("kdp", "kubectl describe pod"),
    ("kds", "kubectl describe svc"),
    ("kdd", "kubectl describe deploy"),
    ("kl", "kubectl logs -f"),
    ("ke", "kubectl exec -it"),
    ("ked", "kubectl edit deploy"),
    ("kes", "kubectl edit svc"),
    ("kdel", "kubectl delete"),
    ("kpf", "kubectl port-forward"),
    ("krr", "kubectl rollout restart"),
    ("kctx", "kubectl config use-context"),
    ("kns", "kubectl config set-context --current --namespace"),
    ("h", "helm"),
    ("hl", "helm list"),
    ("hs", "helm status"),
    ("hh", "helm history"),
];

/// Expands an alias in the first word (`ked web` -> `kubectl edit deploy web`).
pub fn expand_alias(line: &str) -> String {
    let trimmed = line.trim_start();
    let (first, rest) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
    match ALIASES.iter().find(|(alias, _)| *alias == first) {
        Some((_, expansion)) if rest.is_empty() && !line.ends_with(' ') => expansion.to_string(),
        Some((_, expansion)) => format!("{expansion} {rest}"),
        None => line.to_string(),
    }
}

/// An XDG base directory: `$<env_var>` if set, otherwise `$HOME/<fallback...>`.
pub fn xdg_dir(env_var: &str, fallback: &[&str]) -> Option<std::path::PathBuf> {
    let from_env = |var: &str| std::env::var_os(var).filter(|v| !v.is_empty()).map(std::path::PathBuf::from);
    from_env(env_var).or_else(|| from_env("HOME").map(|home| fallback.iter().fold(home, |dir, part| dir.join(part))))
}
