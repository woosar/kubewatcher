use std::path::Path;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::data_structures::Theme;
use crate::helpers::{self, argv};

const LABELS: [&str; 4] = ["Name", "Server", "Namespace", "Token"];
const HINTS: [&str; 4] = [
    "unique across all kubeconfig files, e.g. homelab",
    "IP, host or URL; 192.168.1.20 → https://192.168.1.20:6443",
    "empty means default",
    "bearer token (service account / kubeadm)",
];
const TOKEN: usize = 3;
/// Shown instead of the token in the journal and the status bar.
pub const TOKEN_MASK: &str = "--token=••••";

/// What the form wants after a key press.
#[derive(Debug, PartialEq)]
pub enum FormAction {
    None,
    Cancel,
    Submit,
}

/// The "add context" popup: the four values a kubeconfig entry needs, written as a
/// cluster, a user and a context of the same name into a kubeconfig file of its own.
#[derive(Default)]
pub struct ContextForm {
    fields: [String; 4],
    focus: usize,
}

impl ContextForm {
    pub fn handle_key(&mut self, key: KeyEvent) -> FormAction {
        match key.code {
            KeyCode::Esc => return FormAction::Cancel,
            KeyCode::Enter if self.focus == LABELS.len() - 1 => return FormAction::Submit,
            KeyCode::Enter | KeyCode::Tab | KeyCode::Down => self.focus = (self.focus + 1) % LABELS.len(),
            KeyCode::BackTab | KeyCode::Up => self.focus = (self.focus + LABELS.len() - 1) % LABELS.len(),
            _ => {
                helpers::edit_input(&mut self.fields[self.focus], key);
            }
        }
        FormAction::None
    }

    pub fn name(&self) -> &str {
        self.fields[0].trim()
    }

    /// The three `kubectl config` commands that write the entry into `file`, or what is wrong.
    /// There is no CA to pin, so the cluster is added with TLS verification off.
    pub fn commands(&self, file: &Path) -> Result<[Vec<String>; 3], String> {
        let [name, server, namespace, token] = self.fields.each_ref().map(|f| f.trim());
        let valid_name = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c));
        if !valid_name(name) {
            return Err(format!("name '{name}': use letters, digits and - _ . @ only"));
        }
        if server.is_empty() || server.contains(char::is_whitespace) {
            return Err("server: enter the API server's IP, host or URL".into());
        }
        let namespace = if namespace.is_empty() { "default" } else { namespace };
        if !namespace.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(format!("namespace '{namespace}': lowercase letters, digits and - only"));
        }
        if token.is_empty() || token.contains(char::is_whitespace) {
            return Err("token: paste the bearer token (no spaces)".into());
        }
        let kubeconfig = format!("--kubeconfig={}", file.display());
        let config = |args: &[&str]| {
            let mut argv = argv(&["kubectl", "config"]);
            argv.extend(args.iter().map(|a| a.to_string()));
            argv.push(kubeconfig.clone());
            argv
        };
        let server = format!("--server={}", Self::server_url(server));
        let token = format!("--token={token}");
        let cluster = format!("--cluster={name}");
        let user = format!("--user={name}");
        let namespace = format!("--namespace={namespace}");
        Ok([
            config(&["set-cluster", name, &server, "--insecure-skip-tls-verify=true"]),
            config(&["set-credentials", name, &token]),
            config(&["set-context", name, &cluster, &user, &namespace]),
        ])
    }

    /// `10.0.0.5` → `https://10.0.0.5:6443`, `host:443` → `https://host:443`, `::1` → `https://[::1]:6443`,
    /// URLs stay as they are.
    fn server_url(server: &str) -> String {
        if server.contains("://") {
            return server.to_string();
        }
        // The last colon separates a port, unless it is part of a bare IPv6 address.
        let has_port = server.rsplit_once(':').is_some_and(|(host, port)| port.parse::<u16>().is_ok() && (!host.contains(':') || host.ends_with(']')));
        match (has_port, server.contains(':')) {
            (true, _) => format!("https://{server}"),
            (false, true) => format!("https://[{server}]:6443"),
            (false, false) => format!("https://{server}:6443"),
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, file: &str, theme: &Theme) {
        let mut text = Vec::new();
        for (i, label) in LABELS.iter().enumerate() {
            let focused = i == self.focus;
            let value = if i == TOKEN { "•".repeat(self.fields[i].chars().count().min(40)) } else { self.fields[i].clone() };
            let label_style = if focused { theme.fg(theme.accent).bold() } else { theme.fg(theme.text) };
            let mut spans = vec![Span::styled(format!(" {label:<10}"), label_style), Span::raw(format!("> {value}"))];
            if focused {
                spans.push(Span::styled("█", theme.fg(theme.accent)));
            }
            text.push(Line::from(spans));
            text.push(Line::styled(format!("             {}", HINTS[i]), theme.fg(theme.muted)));
        }
        text.push(Line::from(""));
        text.push(Line::styled(format!(" saved to {file} (TLS not verified)"), theme.fg(theme.muted)));
        text.push(Line::styled(" Tab/↑↓ field · Enter next / save on Token · Esc cancel", theme.fg(theme.muted)));

        let height = text.len() as u16 + 2;
        let popup = area.centered(Constraint::Length(76.min(area.width)), Constraint::Length(height.min(area.height)));
        let block = Block::bordered().style(theme.popup()).title(" Add context ").border_style(theme.fg(theme.accent));
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(text).block(block), popup);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyModifiers;

    use super::*;

    fn type_in(form: &mut ContextForm, fields: [&str; 4]) {
        for value in fields {
            for c in value.chars() {
                form.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
            form.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        }
    }

    #[test]
    fn keys_move_between_fields_and_submit_on_the_last() {
        let mut form = ContextForm::default();
        let press = |form: &mut ContextForm, code| form.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(press(&mut form, KeyCode::Enter), FormAction::None);
        assert_eq!(press(&mut form, KeyCode::Up), FormAction::None);
        assert_eq!(form.focus, 0);
        assert_eq!(press(&mut form, KeyCode::BackTab), FormAction::None);
        assert_eq!(press(&mut form, KeyCode::Enter), FormAction::Submit);
        assert_eq!(press(&mut form, KeyCode::Esc), FormAction::Cancel);
    }

    #[test]
    fn commands_write_cluster_user_and_context() {
        let mut form = ContextForm::default();
        type_in(&mut form, [" homelab ", "192.168.1.20", "", "abc.def"]);
        assert_eq!(form.name(), "homelab");
        let commands = form.commands(Path::new("/h/.kube/config.d/kubewatcher.yaml")).unwrap().map(|c| helpers::shell_join(&c));
        let file = "--kubeconfig=/h/.kube/config.d/kubewatcher.yaml";
        assert_eq!(commands[0], format!("kubectl config set-cluster homelab --server=https://192.168.1.20:6443 --insecure-skip-tls-verify=true {file}"));
        assert_eq!(commands[1], format!("kubectl config set-credentials homelab --token=abc.def {file}"));
        assert_eq!(commands[2], format!("kubectl config set-context homelab --cluster=homelab --user=homelab --namespace=default {file}"));
    }

    #[test]
    fn commands_reject_bad_input() {
        let check = |fields: [&str; 4]| {
            let mut form = ContextForm::default();
            type_in(&mut form, fields);
            form.commands(Path::new("f")).unwrap_err()
        };
        assert!(check(["", "h", "", "t"]).starts_with("name"));
        assert!(check(["a b", "h", "", "t"]).starts_with("name"));
        assert!(check(["a", "", "", "t"]).starts_with("server"));
        assert!(check(["a", "h", "Prod", "t"]).starts_with("namespace"));
        assert!(check(["a", "h", "", ""]).starts_with("token"));
    }

    #[test]
    fn server_url_adds_scheme_and_default_port() {
        assert_eq!(ContextForm::server_url("pi.local"), "https://pi.local:6443");
        assert_eq!(ContextForm::server_url("pi.local:443"), "https://pi.local:443");
        assert_eq!(ContextForm::server_url("[::1]:8443"), "https://[::1]:8443");
        assert_eq!(ContextForm::server_url("::1"), "https://[::1]:6443");
        assert_eq!(ContextForm::server_url("http://x:80"), "http://x:80");
    }
}
