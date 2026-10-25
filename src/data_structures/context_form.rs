use std::path::Path;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::data_structures::Theme;
use crate::helpers;

const LABELS: [&str; 5] = ["Name", "Server", "Namespace", "Cert data", "Key data"];
const HINTS: [&str; 5] = [
    "unique across all kubeconfig files, e.g. teststand1",
    "IP, host or URL; 10.5.15.183 → https://10.5.15.183:6443",
    "empty means default",
    "client-certificate-data (base64, one line)",
    "client-key-data (base64, one line)",
];
/// The secret fields: client certificate and client key.
const SECRETS: [usize; 2] = [3, 4];

/// What the form wants after a key press.
#[derive(Debug, PartialEq)]
pub enum FormAction {
    None,
    Cancel,
    Submit,
}

/// The "add context" popup: what a kubeconfig entry needs, written as a cluster, a user
/// (client certificate + key) and a context of the same name into a kubeconfig file of its own.
#[derive(Default)]
pub struct ContextForm {
    fields: [String; 5],
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
    pub fn commands(&self, file: &Path) -> Result<[Vec<String>; 4], String> {
        let [name, server, namespace, certificate, key] = self.fields.each_ref().map(|f| f.trim());
        // The name becomes part of `users.<name>.…`, where a dot would split the path.
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_@".contains(c)) {
            return Err(format!("name '{name}': use letters, digits and - _ @ only"));
        }
        if server.is_empty() || server.contains(char::is_whitespace) {
            return Err("server: enter the API server's IP, host or URL".into());
        }
        let namespace = if namespace.is_empty() { "default" } else { namespace };
        if !namespace.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(format!("namespace '{namespace}': lowercase letters, digits and - only"));
        }
        if certificate.is_empty() || certificate.contains(char::is_whitespace) {
            return Err("cert data: paste client-certificate-data as one base64 line".into());
        }
        if key.is_empty() || key.contains(char::is_whitespace) {
            return Err("key data: paste client-key-data as one base64 line".into());
        }
        Ok(helpers::context_commands(file, name, &helpers::server_url(server), namespace, certificate, key))
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
        text.push(Line::styled(" Tab/↑↓ field · Enter next / save on Key data (or Token) · Esc cancel", theme.fg(theme.muted)));

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

    fn type_in(form: &mut ContextForm, fields: [&str; 5]) {
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
    fn commands_write_cluster_client_certificate_key_and_context() {
        let mut form = ContextForm::default();
        type_in(&mut form, [" teststand1 ", "10.5.15.183", "", "Q0VSVA==", "S0VZ"]);
        assert_eq!(form.name(), "teststand1");
        let commands = form.commands(Path::new("/h/.kube/config.d/teststand1.yaml")).unwrap().map(|c| helpers::shell_join(&c));
        let file = "--kubeconfig=/h/.kube/config.d/teststand1.yaml";
        assert_eq!(
            commands,
            [
                format!("kubectl config set-cluster teststand1 --server=https://10.5.15.183:6443 --insecure-skip-tls-verify=true {file}"),
                format!("kubectl config set users.teststand1.client-certificate-data Q0VSVA== {file}"),
                format!("kubectl config set users.teststand1.client-key-data S0VZ {file}"),
                format!("kubectl config set-context teststand1 --cluster=teststand1 --user=teststand1 --namespace=default {file}"),
            ]
        );
    }

    #[test]
    fn commands_reject_bad_input() {
        let check = |fields: [&str; 5]| {
            let mut form = ContextForm::default();
            type_in(&mut form, fields);
            form.commands(Path::new("f")).unwrap_err()
        };
        assert!(check(["", "h", "", "c", "k"]).starts_with("name"));
        assert!(check(["a b", "h", "", "c", "k"]).starts_with("name"));
        assert!(check(["a.b", "h", "", "c", "k"]).starts_with("name"));
        assert!(check(["a", "", "", "c", "k"]).starts_with("server"));
        assert!(check(["a", "h", "Prod", "c", "k"]).starts_with("namespace"));
        assert!(check(["a", "h", "", "", "k"]).starts_with("cert data"));
        assert!(check(["a", "h", "", "c", ""]).starts_with("key data"));
    }
}
