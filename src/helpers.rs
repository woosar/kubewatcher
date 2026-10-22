use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::RwLock;
use std::sync::mpsc::Sender;
use std::thread;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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

/// Environment every started command gets on top of kubewatcher's own (PATH, KUBECONFIG).
/// Kept here instead of `set_var`, which is unsound once threads run.
static CHILD_ENV: RwLock<Vec<(&'static str, OsString)>> = RwLock::new(Vec::new());

/// Sets a variable for every command started from now on.
pub fn set_child_env(name: &'static str, value: OsString) {
    let mut env = CHILD_ENV.write().unwrap_or_else(|e| e.into_inner());
    env.retain(|(n, _)| *n != name);
    env.push((name, value));
}

/// A variable as the started commands see it.
pub fn child_env(name: &str) -> Option<OsString> {
    let env = CHILD_ENV.read().unwrap_or_else(|e| e.into_inner());
    env.iter().find(|(n, _)| *n == name).map(|(_, v)| v.clone()).or_else(|| std::env::var_os(name))
}

/// The one way commands are built: argv plus the child environment. A changed PATH is
/// also where the program itself is looked up.
pub fn command(argv: &[String]) -> Command {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    for (name, value) in CHILD_ENV.read().unwrap_or_else(|e| e.into_inner()).iter() {
        command.env(name, value);
    }
    command
}

/// Runs a command to completion; Ok(stdout) on success, Err(stderr) otherwise.
pub fn run_capture(argv: &[String]) -> Result<String, String> {
    let output = command(argv)
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
    let mut child = command(argv)
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

fn forward_lines(reader: impl Read + Send + 'static, generation: u64, tx: Sender<Msg>, signal_end: bool) -> thread::JoinHandle<()> {
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
    })
}

/// Spawns a detached helper process (e.g. port-forward); stderr is kept for diagnostics.
pub fn spawn_background(argv: &[String]) -> io::Result<Child> {
    command(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
}

/// Turns the exit status of an interactive command into Ok or a readable error.
pub fn exit_outcome(argv: &[String], status: io::Result<ExitStatus>) -> Result<(), String> {
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(status.to_string()),
        Err(e) => Err(format!("failed to run `{}`: {e}", argv[0])),
    }
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

/// Prompt aliases, following the oh-my-zsh kubectl/helm plugins so shell muscle memory works here too.
pub const ALIASES: &[(&str, &str)] = &[
    ("k", "kubectl"),
    ("kaf", "kubectl apply -f"),
    ("kdel", "kubectl delete"),
    ("kdelf", "kubectl delete -f"),
    ("kd", "kubectl describe"),
    ("kg", "kubectl get"),
    ("kga", "kubectl get all"),
    ("kgaa", "kubectl get all --all-namespaces"),
    ("ke", "kubectl exec -it"),
    ("keti", "kubectl exec -t -i"),
    ("kcp", "kubectl cp"),
    ("kpf", "kubectl port-forward"),
    // contexts & namespaces
    ("kccc", "kubectl config current-context"),
    ("kcgc", "kubectl config get-contexts"),
    ("kcuc", "kubectl config use-context"),
    ("kctx", "kubectl config use-context"),
    ("kcn", "kubectl config set-context --current --namespace"),
    ("kns", "kubectl config set-context --current --namespace"),
    ("kgns", "kubectl get namespaces"),
    ("kdns", "kubectl describe namespace"),
    // logs
    ("kl", "kubectl logs"),
    ("klf", "kubectl logs -f"),
    ("kl1h", "kubectl logs --since=1h"),
    ("kl1m", "kubectl logs --since=1m"),
    ("kl1s", "kubectl logs --since=1s"),
    ("klf1h", "kubectl logs -f --since=1h"),
    ("klf1m", "kubectl logs -f --since=1m"),
    ("klf1s", "kubectl logs -f --since=1s"),
    // pods
    ("kgp", "kubectl get pods"),
    ("kgpa", "kubectl get pods --all-namespaces"),
    ("kgpw", "kubectl get pods --watch"),
    ("kgpwide", "kubectl get pods -o wide"),
    ("kgpl", "kubectl get pods -l"),
    ("kgpn", "kubectl get pods -n"),
    ("kdp", "kubectl describe pods"),
    ("kep", "kubectl edit pods"),
    ("kdelp", "kubectl delete pods"),
    // services
    ("kgs", "kubectl get svc"),
    ("kgsa", "kubectl get svc --all-namespaces"),
    ("kgsw", "kubectl get svc --watch"),
    ("kgswide", "kubectl get svc -o wide"),
    ("kds", "kubectl describe svc"),
    ("kes", "kubectl edit svc"),
    ("kdels", "kubectl delete svc"),
    // ingress
    ("kgi", "kubectl get ingress"),
    ("kgia", "kubectl get ingress --all-namespaces"),
    ("kdi", "kubectl describe ingress"),
    ("kei", "kubectl edit ingress"),
    ("kdeli", "kubectl delete ingress"),
    // deployments
    ("kgd", "kubectl get deployment"),
    ("kgda", "kubectl get deployment --all-namespaces"),
    ("kgdw", "kubectl get deployment --watch"),
    ("kgdwide", "kubectl get deployment -o wide"),
    ("kdd", "kubectl describe deployment"),
    ("ked", "kubectl edit deployment"),
    ("kdeld", "kubectl delete deployment"),
    ("ksd", "kubectl scale deployment"),
    ("krsd", "kubectl rollout status deployment"),
    ("krr", "kubectl rollout restart"),
    ("krh", "kubectl rollout history"),
    ("kru", "kubectl rollout undo"),
    // statefulsets, daemonsets, replicasets
    ("kgss", "kubectl get statefulset"),
    ("kgssa", "kubectl get statefulset --all-namespaces"),
    ("kdss", "kubectl describe statefulset"),
    ("kess", "kubectl edit statefulset"),
    ("ksss", "kubectl scale statefulset"),
    ("krsss", "kubectl rollout status statefulset"),
    ("kgds", "kubectl get daemonset"),
    ("kdds", "kubectl describe daemonset"),
    ("kgrs", "kubectl get replicaset"),
    ("kdrs", "kubectl describe replicaset"),
    // config & storage
    ("kgcm", "kubectl get configmaps"),
    ("kgcma", "kubectl get configmaps --all-namespaces"),
    ("kdcm", "kubectl describe configmap"),
    ("kecm", "kubectl edit configmap"),
    ("kgsec", "kubectl get secret"),
    ("kgseca", "kubectl get secret --all-namespaces"),
    ("kdsec", "kubectl describe secret"),
    ("kgpvc", "kubectl get pvc"),
    ("kgpvca", "kubectl get pvc --all-namespaces"),
    ("kdpvc", "kubectl describe pvc"),
    // jobs & nodes
    ("kgj", "kubectl get job"),
    ("kdj", "kubectl describe job"),
    ("kgcj", "kubectl get cronjob"),
    ("kdcj", "kubectl describe cronjob"),
    ("kgno", "kubectl get nodes"),
    ("kdno", "kubectl describe node"),
    // helm
    ("h", "helm"),
    ("hl", "helm list"),
    ("hs", "helm status"),
    ("hh", "helm history"),
    ("hin", "helm install"),
    ("hup", "helm upgrade"),
    ("hun", "helm uninstall"),
    ("hse", "helm search"),
];

/// Expands an alias in the first word (`ked web` -> `kubectl edit deployment web`).
/// `watch [-n N] <alias> ...` expands the alias after the watch prefix.
pub fn expand_alias(line: &str) -> String {
    let trimmed = line.trim_start();
    let words: Vec<&str> = trimmed.split(' ').collect();
    let skip = match words.as_slice() {
        ["watch", "-n", _, _, ..] => 3,
        ["watch", next, ..] if *next != "-n" => 1,
        _ => 0,
    };
    let prefix: String = words[..skip].iter().map(|w| format!("{w} ")).collect();
    let rest_line = words[skip..].join(" ");
    let (first, rest) = rest_line.split_once(' ').unwrap_or((rest_line.as_str(), ""));
    match ALIASES.iter().find(|(alias, _)| *alias == first) {
        Some((_, expansion)) if rest.is_empty() && !line.ends_with(' ') => format!("{prefix}{expansion}"),
        Some((_, expansion)) => format!("{prefix}{expansion} {rest}"),
        None => line.to_string(),
    }
}

/// An XDG base directory: `$<env_var>` if set, otherwise `$HOME/<fallback...>`.
pub fn xdg_dir(env_var: &str, fallback: &[&str]) -> Option<std::path::PathBuf> {
    let from_env = |var: &str| std::env::var_os(var).filter(|v| !v.is_empty()).map(std::path::PathBuf::from);
    from_env(env_var).or_else(|| from_env("HOME").map(|home| fallback.iter().fold(home, |dir, part| dir.join(part))))
}

/// `~/.kube/config` followed by every `~/.kube/config.d/*.yaml` in name order, like the
/// shell's `kubeconfig_rebuild`; None without a config.d directory (KUBECONFIG stays as it is).
pub fn rebuild_kubeconfig(home: &Path) -> Option<OsString> {
    let config_d = home.join(".kube").join("config.d");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&config_d)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension() == Some(OsStr::new("yaml")) && path.is_file())
        .collect();
    files.sort();
    files.insert(0, home.join(".kube").join("config"));
    std::env::join_paths(files).ok()
}

/// The kubeconfig files kubectl reads, in order: `$KUBECONFIG` or `~/.kube/config`.
pub fn kubeconfig_files(home: &Path) -> Vec<PathBuf> {
    match child_env("KUBECONFIG").filter(|v| !v.is_empty()) {
        Some(value) => std::env::split_paths(&value).collect(),
        None => vec![home.join(".kube").join("config")],
    }
}

/// A file name for a context: anything but letters, digits, `.`, `_` and `-` becomes `-`.
pub fn context_file_name(name: &str) -> String {
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect();
    let safe = safe.trim_matches('.').to_string();
    if safe.is_empty() { "context".into() } else { safe }
}

/// `<dir>/<context_file_name>.yaml`, with `-2`, `-3`… when that file already exists.
pub fn context_file_path(dir: &Path, name: &str) -> PathBuf {
    let base = context_file_name(name);
    (1..)
        .map(|n| dir.join(if n == 1 { format!("{base}.yaml") } else { format!("{base}-{n}.yaml") }))
        .find(|path| !path.exists())
        .unwrap_or_else(|| dir.join(format!("{base}.yaml")))
}

/// Writes a file only its owner can read (kubeconfigs hold credentials).
pub fn write_private(path: &Path, content: &str) -> io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    // `mode` only applies to new files.
    #[cfg(unix)]
    file.set_permissions(<std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600))?;
    file.write_all(content.as_bytes())
}

/// The first executable called `name` on `search_path`.
pub fn find_program(name: &str, search_path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(search_path).map(|dir| dir.join(name)).find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    path.is_file()
}

/// The first version number in a tool's output: `Client Version: v1.34.1` → `1.34.1`.
pub fn first_version(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || "(),:\"".contains(c)).find_map(|word| {
        let digits: String = word.trim_start_matches('v').chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        let digits = digits.trim_end_matches('.');
        (digits.contains('.') && digits.starts_with(|c: char| c.is_ascii_digit())).then(|| digits.to_string())
    })
}

/// `v1.2.3` and `1.2.3` are the same version.
pub fn same_version(a: &str, b: &str) -> bool {
    a.trim().trim_start_matches('v') == b.trim().trim_start_matches('v')
}

/// The single-line `plugins=(...)` of a .zshrc, if there is exactly one.
fn omz_plugins_line(rc: &str) -> Option<(usize, &str)> {
    let lines: Vec<(usize, &str)> = rc.lines().enumerate().filter(|(_, l)| l.trim_start().starts_with("plugins=(")).collect();
    match lines[..] {
        [(index, line)] if line.trim_end().ends_with(')') => Some((index, line)),
        _ => None,
    }
}

pub fn omz_has_plugin(rc: &str, plugin: &str) -> bool {
    rc.lines()
        .filter(|l| l.trim_start().starts_with("plugins=("))
        .any(|l| l.trim().trim_start_matches("plugins=(").trim_end_matches(')').split_whitespace().any(|p| p == plugin))
}

/// The rc with `plugin` added to oh-my-zsh's `plugins=(...)`, or None if it is already
/// there or the line cannot be edited safely (several lines, multi-line list).
pub fn add_omz_plugin(rc: &str, plugin: &str) -> Option<String> {
    if omz_has_plugin(rc, plugin) {
        return None;
    }
    let (index, line) = omz_plugins_line(rc)?;
    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
    let inner = line.trim().trim_start_matches("plugins=(").trim_end_matches(')').trim();
    let new_line = if inner.is_empty() { format!("{indent}plugins=({plugin})") } else { format!("{indent}plugins=({inner} {plugin})") };
    let text = rc.lines().enumerate().map(|(i, l)| if i == index { new_line.as_str() } else { l }).collect::<Vec<_>>().join("\n");
    Some(text + if rc.ends_with('\n') { "\n" } else { "" })
}

/// `text` with the marked block replaced, or appended when it is not there yet.
/// `block` includes both marker lines.
pub fn with_block(text: &str, open: &str, close: &str, block: &str) -> String {
    if let (Some(start), Some(end)) = (text.find(open), text.find(close))
        && start < end
    {
        let after = text[end + close.len()..].strip_prefix('\n').unwrap_or(&text[end + close.len()..]);
        return format!("{}{block}{after}", &text[..start]);
    }
    let separator = match text {
        "" => "",
        t if t.ends_with("\n\n") => "",
        t if t.ends_with('\n') => "\n",
        _ => "\n\n",
    };
    format!("{text}{separator}{block}")
}

/// A fresh, not yet created directory under the system temp dir for one test.
#[cfg(test)]
pub fn test_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("kubewatcher-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn builds_and_joins_argv() {
        assert_eq!(shell("echo hi"), ["sh", "-c", "echo hi"]);
        assert_eq!(shell_join(&shell("echo hi | wc")), "echo hi | wc");
        let args = argv(&["kubectl", "get", "pods", "-o", "jsonpath={.items[*]}", "", "it's"]);
        assert_eq!(shell_join(&args), r"kubectl get pods -o 'jsonpath={.items[*]}' '' 'it'\''s'");
    }

    #[test]
    fn run_capture_reports_stdout_or_the_best_error() {
        assert_eq!(run_capture(&shell("echo out")), Ok("out\n".into()));
        assert_eq!(run_capture(&shell("echo out; echo bad >&2; exit 1")), Err("bad".into()));
        assert_eq!(run_capture(&shell("echo only-stdout; exit 1")), Err("only-stdout".into()));
        assert!(run_capture(&shell("exit 3")).unwrap_err().starts_with("exited with"));
        assert!(run_capture(&argv(&["kubewatcher-no-such-program"])).unwrap_err().starts_with("failed to run"));
    }

    #[test]
    fn spawn_capture_sends_the_result() {
        let (tx, rx) = mpsc::channel();
        spawn_capture(shell("echo done"), Target::Action, 7, tx);
        let expected = Msg::Output { target: Target::Action, journal: 7, result: Ok("done\n".into()) };
        assert_eq!(rx.recv().unwrap(), expected);
    }

    #[test]
    fn spawn_stream_forwards_stdout_and_stderr_lines() {
        let (tx, rx) = mpsc::channel();
        let mut child = spawn_stream(&shell("echo out; echo err >&2"), 3, &tx).unwrap();
        child.wait().unwrap();
        drop(tx);
        // stdout and stderr are read by separate threads, so only the set is fixed.
        let messages: Vec<Msg> = rx.iter().collect();
        assert_eq!(messages.len(), 3);
        assert!(messages.contains(&Msg::Line { generation: 3, line: "out".into() }));
        assert!(messages.contains(&Msg::Line { generation: 3, line: "err".into() }));
        assert!(messages.contains(&Msg::StreamEnd { generation: 3 }));
        assert!(spawn_stream(&argv(&["kubewatcher-no-such-program"]), 1, &mpsc::channel().0).is_err());
    }

    /// A reader that fails, like a pipe whose process vanished.
    struct Broken;

    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("broken"))
        }
    }

    #[test]
    fn forward_lines_stops_on_errors_and_closed_receivers() {
        let (tx, rx) = mpsc::channel();
        forward_lines(io::Cursor::new(b"a\r\nb".to_vec()), 1, tx.clone(), true).join().unwrap();
        forward_lines(Broken, 1, tx, false).join().unwrap();
        let line = |text: &str| Msg::Line { generation: 1, line: text.into() };
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [line("a"), line("b"), Msg::StreamEnd { generation: 1 }]);

        let (tx, rx) = mpsc::channel();
        drop(rx);
        forward_lines(io::Cursor::new(b"ignored\n".to_vec()), 1, tx, true).join().unwrap();
    }

    #[test]
    fn spawn_background_starts_a_process() {
        let mut child = spawn_background(&shell("exit 0")).unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn exit_outcome_describes_failures() {
        let status = |script: &str| Command::new("sh").args(["-c", script]).status();
        let args = argv(&["kubectl", "edit"]);
        assert_eq!(exit_outcome(&args, status("exit 0")), Ok(()));
        assert!(exit_outcome(&args, status("exit 2")).unwrap_err().contains('2'));
        assert_eq!(exit_outcome(&args, Err(io::Error::other("gone"))), Err("failed to run `kubectl`: gone".into()));
    }

    #[test]
    fn sanitize_line_strips_escapes_and_controls() {
        assert_eq!(sanitize_line("\x1b[31mred\x1b[0m\tx\x1bM\x07!"), "red    x!");
    }

    #[test]
    fn selection_helpers_stay_in_range() {
        assert_eq!(step_selection(None, 0, 1), None);
        assert_eq!(step_selection(None, 3, 1), Some(1));
        assert_eq!(step_selection(Some(2), 3, 5), Some(2));
        assert_eq!(step_selection(Some(1), 3, -5), Some(0));
        assert_eq!(clamp_selection(Some(4), 0), None);
        assert_eq!(clamp_selection(None, 2), Some(0));
        assert_eq!(clamp_selection(Some(4), 2), Some(1));
    }

    #[test]
    fn edit_input_handles_line_editing_keys() {
        let mut buf = "kubectl get  ".to_string();
        assert!(edit_input(&mut buf, key(KeyCode::Char('w'), KeyModifiers::CONTROL)));
        assert_eq!(buf, "kubectl ");
        assert!(edit_input(&mut buf, key(KeyCode::Char('x'), KeyModifiers::NONE)));
        assert!(edit_input(&mut buf, key(KeyCode::Backspace, KeyModifiers::NONE)));
        assert_eq!(buf, "kubectl ");
        assert!(!edit_input(&mut buf, key(KeyCode::Char('x'), KeyModifiers::CONTROL)));
        assert!(!edit_input(&mut buf, key(KeyCode::Left, KeyModifiers::NONE)));
        assert!(edit_input(&mut buf, key(KeyCode::Char('u'), KeyModifiers::CONTROL)));
        assert_eq!(buf, "");
    }

    #[test]
    fn matches_filter_is_case_insensitive() {
        assert!(matches_filter(["anything"], ""));
        assert!(matches_filter(["web-7d9f", "Running"], "RUN"));
        assert!(!matches_filter(["web-7d9f"], "db"));
    }

    #[test]
    fn expand_alias_keeps_trailing_space_and_unknown_words() {
        assert_eq!(expand_alias("k"), "kubectl");
        assert_eq!(expand_alias("kgp "), "kubectl get pods ");
        assert_eq!(expand_alias("klf web"), "kubectl logs -f web");
        assert_eq!(expand_alias("watch"), "watch");
        assert_eq!(expand_alias("ls -la"), "ls -la");
    }

    #[test]
    fn child_env_reaches_started_commands() {
        set_child_env("KUBEWATCHER_TEST_CHILD_ENV", "seen".into());
        assert_eq!(run_capture(&shell("echo $KUBEWATCHER_TEST_CHILD_ENV")), Ok("seen\n".into()));
        assert_eq!(child_env("KUBEWATCHER_TEST_CHILD_ENV"), Some("seen".into()));
        set_child_env("KUBEWATCHER_TEST_CHILD_ENV", "again".into());
        assert_eq!(child_env("KUBEWATCHER_TEST_CHILD_ENV"), Some("again".into()));
    }

    #[test]
    fn kubeconfig_is_rebuilt_from_config_d() {
        let home = test_dir("rebuild");
        assert_eq!(rebuild_kubeconfig(&home), None);
        let config_d = home.join(".kube/config.d");
        std::fs::create_dir_all(&config_d).unwrap();
        for name in ["b.yaml", "a.yaml", "notes.txt", "EXAMPLE.yaml.template"] {
            std::fs::write(config_d.join(name), "").unwrap();
        }
        let files: Vec<PathBuf> = std::env::split_paths(&rebuild_kubeconfig(&home).unwrap()).collect();
        assert_eq!(files, [home.join(".kube/config"), config_d.join("a.yaml"), config_d.join("b.yaml")]);
        assert_eq!(context_file_path(&config_d, "a"), config_d.join("a-2.yaml"));
        assert_eq!(context_file_path(&config_d, "c"), config_d.join("c.yaml"));

        write_private(&config_d.join("secret.yaml"), "token").unwrap();
        assert_eq!(std::fs::read_to_string(config_d.join("secret.yaml")).unwrap(), "token");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(config_d.join("secret.yaml")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn context_file_names_are_safe() {
        assert_eq!(context_file_name("homelab"), "homelab");
        assert_eq!(context_file_name("arn:aws:eks:eu/prod"), "arn-aws-eks-eu-prod");
        assert_eq!(context_file_name("admin@k3s.local"), "admin-k3s.local");
        assert_eq!(context_file_name(".."), "context");
    }

    #[test]
    fn programs_are_found_on_the_search_path() {
        let dir = test_dir("find-program");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tool"), "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(find_program("tool", dir.as_os_str()), None, "not executable");
            std::fs::set_permissions(dir.join("tool"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(find_program("tool", dir.as_os_str()), Some(dir.join("tool")));
        assert_eq!(find_program("other", dir.as_os_str()), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn versions_are_read_from_tool_output() {
        assert_eq!(first_version("Client Version: v1.34.1\nKustomize Version: v5.7.1"), Some("1.34.1".into()));
        assert_eq!(first_version("0.65.2 (b5e9d4a)"), Some("0.65.2".into()));
        assert_eq!(first_version("flux: v2.7.0"), Some("2.7.0".into()));
        assert_eq!(first_version("no version here 3"), None);
        assert!(same_version("v1.2.3", "1.2.3 ") && !same_version("1.2.3", "1.2.4"));
    }

    #[test]
    fn rc_files_get_blocks_and_plugins() {
        assert_eq!(add_omz_plugin("a\n  plugins=(git)\nb\n", "kubectl"), Some("a\n  plugins=(git kubectl)\nb\n".into()));
        assert_eq!(add_omz_plugin("plugins=()", "kubectl"), Some("plugins=(kubectl)".into()));
        assert_eq!(add_omz_plugin("plugins=(git kubectl)\n", "kubectl"), None);
        assert_eq!(add_omz_plugin("plugins=(\n  git\n)\n", "kubectl"), None);
        assert_eq!(add_omz_plugin("no plugins\n", "kubectl"), None);
        assert!(omz_has_plugin("plugins=(git kubectl helm)", "kubectl") && !omz_has_plugin("plugins=(kubectl-x)", "kubectl"));

        let block = "# >>\nnew\n# <<\n";
        assert_eq!(with_block("", "# >>", "# <<", block), block);
        assert_eq!(with_block("x", "# >>", "# <<", block), format!("x\n\n{block}"));
        assert_eq!(with_block("x\n", "# >>", "# <<", block), format!("x\n\n{block}"));
        assert_eq!(with_block("x\n# >>\nold\n# <<\ny\n", "# >>", "# <<", block), format!("x\n{block}y\n"));
    }

    #[test]
    fn xdg_dir_prefers_the_variable_then_home() {
        assert_eq!(xdg_dir("PATH", &["x"]), std::env::var_os("PATH").map(std::path::PathBuf::from));
        let home = std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"));
        assert_eq!(xdg_dir("KUBEWATCHER_TEST_UNSET_VARIABLE", &[".config"]), home);
    }
}
