use std::path::Path;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::data_structures::Theme;
use crate::helpers::{self, argv};

const LABELS: [&str; 6] = ["Name", "Server", "Namespace", "Token", "Cert data", "Key data"];
const HINTS: [&str; 6] = [
    "unique across all kubeconfig files, e.g. homelab",
    "IP, host or URL; 192.168.1.20 → https://192.168.1.20:6443",
    "empty means default",
    "bearer token (service account / kubeadm), or leave empty and use cert + key",
    "client-certificate-data (base64, as in k3s.yaml)",
    "client-key-data (base64, as in k3s.yaml)",
];
/// The secret fields: token, client certificate, client key.
const SECRETS: [usize; 3] = [3, 4, 5];
/// Shown instead of the token in the journal and the status bar.
pub const TOKEN_MASK: &str = "--token=••••";

/// What the form wants after a key press.
#[derive(Debug, PartialEq)]
pub enum FormAction {
    None,
    Cancel,
    Submit,
}

/// The "add context" popup: what a kubeconfig entry needs, written as a cluster, a user
/// and a context of the same name into a kubeconfig file of its own. The user signs in
/// with a token or with a client certificate and key.
#[derive(Default)]
pub struct ContextForm {
    fields: [String; 6],
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

    /// The `kubectl config` commands that write the entry into `file`, or what is wrong.
    /// There is no CA to pin, so the cluster is added with TLS verification off.
    pub fn commands(&self, file: &Path) -> Result<Vec<Vec<String>>, String> {
        let [name, server, namespace, token, certificate, key] = self.fields.each_ref().map(|f| f.trim());
        let valid_name = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c));
        if !valid_name(name) {
            return Err(format!("name '{name}': use letters, digits and - _ . @ only"));
        }
        let with_certificate = !certificate.is_empty() || !key.is_empty();
        if server.is_empty() || server.contains(char::is_whitespace) {
            return Err("server: enter the API server's IP, host or URL".into());
        }
        let namespace = if namespace.is_empty() { "default" } else { namespace };
        if !namespace.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(format!("namespace '{namespace}': lowercase letters, digits and - only"));
        }
        if with_certificate {
            if !token.is_empty() {
                return Err("token: leave it empty when using cert + key data".into());
            }
            if certificate.is_empty() || key.is_empty() || certificate.contains(char::is_whitespace) || key.contains(char::is_whitespace) {
                return Err("cert/key data: paste both, each as one base64 line".into());
            }
            // The name becomes part of `users.<name>.…`, where a dot would split the path.
            if name.contains('.') {
                return Err(format!("name '{name}': no dots when using cert + key data"));
            }
        } else if token.is_empty() || token.contains(char::is_whitespace) {
            return Err("token: paste the bearer token (no spaces), or cert + key data".into());
        }
        let kubeconfig = format!("--kubeconfig={}", file.display());
        let config = |args: &[&str]| {
            let mut argv = argv(&["kubectl", "config"]);
            argv.extend(args.iter().map(|a| a.to_string()));
            argv.push(kubeconfig.clone());
            argv
        };
        let server = format!("--server={}", helpers::server_url(server));
        let cluster = format!("--cluster={name}");
        let user = format!("--user={name}");
        let namespace = format!("--namespace={namespace}");
        let mut commands = vec![config(&["set-cluster", name, &server, "--insecure-skip-tls-verify=true"])];
        if with_certificate {
            // `config set` base64-decodes the value and stores the bytes, which come out as the same base64 again.
            commands.push(config(&["set", &format!("users.{name}.client-certificate-data"), certificate]));
            commands.push(config(&["set", &format!("users.{name}.client-key-data"), key]));
        } else {
            commands.push(config(&["set-credentials", name, &format!("--token={token}")]));
        }
        commands.push(config(&["set-context", name, &cluster, &user, &namespace]));
        Ok(commands)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, file: &str, theme: &Theme) {
        let mut text = Vec::new();
        for (i, label) in LABELS.iter().enumerate() {
            let focused = i == self.focus;
            let value = if SECRETS.contains(&i) { "•".repeat(self.fields[i].chars().count().min(40)) } else { self.fields[i].clone() };
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
        text.push(Line::styled(" Tab/↑↓ field · Enter next / save on Key data · Esc cancel", theme.fg(theme.muted)));

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

    fn type_in(form: &mut ContextForm, fields: [&str; 6]) {
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
        type_in(&mut form, [" homelab ", "192.168.1.20", "", "abc.def", "", ""]);
        assert_eq!(form.name(), "homelab");
        let commands: Vec<String> = form.commands(Path::new("/h/.kube/config.d/kubewatcher.yaml")).unwrap().iter().map(|c| helpers::shell_join(c)).collect();
        let file = "--kubeconfig=/h/.kube/config.d/kubewatcher.yaml";
        assert_eq!(
            commands,
            [
                format!("kubectl config set-cluster homelab --server=https://192.168.1.20:6443 --insecure-skip-tls-verify=true {file}"),
                format!("kubectl config set-credentials homelab --token=abc.def {file}"),
                format!("kubectl config set-context homelab --cluster=homelab --user=homelab --namespace=default {file}"),
            ]
        );
    }

    #[test]
    fn commands_can_use_client_certificate_and_key_instead_of_a_token() {
        let mut form = ContextForm::default();
        type_in(&mut form, ["teststand1", "10.5.15.183", "", "", "Q0VSVA==", "S0VZ"]);
        let commands: Vec<String> = form.commands(Path::new("/h/t.yaml")).unwrap().iter().map(|c| helpers::shell_join(c)).collect();
        assert_eq!(
            commands[1..3],
            [
                "kubectl config set users.teststand1.client-certificate-data Q0VSVA== --kubeconfig=/h/t.yaml",
                "kubectl config set users.teststand1.client-key-data S0VZ --kubeconfig=/h/t.yaml",
            ]
        );
        assert_eq!(commands.len(), 4);
    }

    #[test]
    fn commands_reject_bad_input() {
        let check = |fields: [&str; 6]| {
            let mut form = ContextForm::default();
            type_in(&mut form, fields);
            form.commands(Path::new("f")).unwrap_err()
        };
        assert!(check(["", "h", "", "t", "", ""]).starts_with("name"));
        assert!(check(["a b", "h", "", "t", "", ""]).starts_with("name"));
        assert!(check(["a", "", "", "t", "", ""]).starts_with("server"));
        assert!(check(["a", "h", "Prod", "t", "", ""]).starts_with("namespace"));
        assert!(check(["a", "h", "", "", "", ""]).starts_with("token"));
        assert!(check(["a", "h", "", "t", "c", "k"]).starts_with("token"));
        assert!(check(["a", "h", "", "", "c", ""]).starts_with("cert/key"));
        assert!(check(["a.b", "h", "", "", "c", "k"]).starts_with("name"));
    }
}
