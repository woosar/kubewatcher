use std::path::Path;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::data_structures::{FormAction, Theme};
use crate::helpers::{self, argv};

const LABELS: [&str; 2] = ["Name", "Host"];
const HINTS: [&str; 2] = [
    "unique across all kubeconfig files, e.g. teststand3",
    "IP of the k3s server; user@IP when your ssh user differs",
];
/// Where k3s writes its admin kubeconfig (root only, hence sudo).
const K3S_YAML: &str = "/etc/rancher/k3s/k3s.yaml";

/// The "import from k3s" popup: name and host. kubewatcher then reads k3s.yaml over ssh
/// (you type the ssh and sudo passwords on the real terminal) and writes a context from
/// its server port, client certificate and client key, with the host's IP as the server.
#[derive(Default)]
pub struct K3sForm {
    fields: [String; 2],
    focus: usize,
}

impl K3sForm {
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

    fn host(&self) -> &str {
        self.fields[1].trim()
    }

    /// The host without the ssh user: what replaces 0.0.0.0 in the server URL.
    fn ip(&self) -> &str {
        self.host().rsplit_once('@').map_or(self.host(), |(_, ip)| ip)
    }

    /// `ssh -t <host> sudo cat /etc/rancher/k3s/k3s.yaml`, or what is wrong with the input.
    /// `-t` gives sudo a terminal to ask for its password on.
    pub fn fetch(&self) -> Result<Vec<String>, String> {
        // The name ends up in `kubectl config set users.<name>.…`, where a dot would split the path.
        let name = self.name();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return Err(format!("name '{name}': use letters, digits, - and _ only"));
        }
        if self.ip().is_empty() || self.host().contains(char::is_whitespace) {
            return Err("host: enter the k3s server's IP (or user@IP)".into());
        }
        Ok(argv(&["ssh", "-t", self.host(), &format!("sudo cat {K3S_YAML}")]))
    }

    /// The `kubectl config` commands that write the context into `file`, from the k3s.yaml
    /// text `fetch` printed (prompts and \r included). The CA is left out, as in the
    /// existing teststand configs, so TLS is not verified.
    pub fn commands(&self, file: &Path, k3s_yaml: &str) -> Result<[Vec<String>; 4], String> {
        let value = |key: &str| {
            k3s_yaml
                .lines()
                .map(str::trim)
                .find_map(|line| line.strip_prefix(key).and_then(|rest| rest.strip_prefix(':')))
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("no {key} in the output of sudo cat {K3S_YAML} (wrong password or not a k3s server?)"))
        };
        let server = value("server")?;
        let certificate = value("client-certificate-data")?;
        let key = value("client-key-data")?;
        // https://0.0.0.0:6443 → https://<ip>:6443
        let port = server.rsplit_once(':').map(|(_, p)| p).filter(|p| p.parse::<u16>().is_ok()).unwrap_or("6443");
        let ip = self.ip();
        let address = if ip.contains(':') && !ip.starts_with('[') { format!("[{ip}]:{port}") } else { format!("{ip}:{port}") };

        let name = self.name();
        let kubeconfig = format!("--kubeconfig={}", file.display());
        let config = |args: &[&str]| {
            let mut argv = argv(&["kubectl", "config"]);
            argv.extend(args.iter().map(|a| a.to_string()));
            argv.push(kubeconfig.clone());
            argv
        };
        let server = format!("--server={}", helpers::server_url(&address));
        let cluster = format!("--cluster={name}");
        let user = format!("--user={name}");
        Ok([
            config(&["set-cluster", name, &server, "--insecure-skip-tls-verify=true"]),
            // `config set` base64-decodes the value and stores the bytes, which come out as the same base64 again.
            config(&["set", &format!("users.{name}.client-certificate-data"), certificate]),
            config(&["set", &format!("users.{name}.client-key-data"), key]),
            config(&["set-context", name, &cluster, &user, "--namespace=default"]),
        ])
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, file: &str, theme: &Theme) {
        let mut text = Vec::new();
        for (i, label) in LABELS.iter().enumerate() {
            let focused = i == self.focus;
            let label_style = if focused { theme.fg(theme.accent).bold() } else { theme.fg(theme.text) };
            let mut spans = vec![Span::styled(format!(" {label:<10}"), label_style), Span::raw(format!("> {}", self.fields[i]))];
            if focused {
                spans.push(Span::styled("█", theme.fg(theme.accent)));
            }
            text.push(Line::from(spans));
            text.push(Line::styled(format!("             {}", HINTS[i]), theme.fg(theme.muted)));
        }
        text.push(Line::from(""));
        let host = if self.host().is_empty() { "<host>" } else { self.host() };
        text.push(Line::styled(format!(" runs ssh -t {host} sudo cat {K3S_YAML}"), theme.fg(theme.muted)));
        text.push(Line::styled(" (you type the ssh and sudo passwords), keeps the client cert and key,", theme.fg(theme.muted)));
        text.push(Line::styled(format!(" saved to {file} (TLS not verified)"), theme.fg(theme.muted)));
        text.push(Line::styled(" Tab/↑↓ field · Enter next / fetch on Host · Esc cancel", theme.fg(theme.muted)));

        let height = text.len() as u16 + 2;
        let popup = area.centered(Constraint::Length(76.min(area.width)), Constraint::Length(height.min(area.height)));
        let block = Block::bordered().style(theme.popup()).title(" Import context from k3s ").border_style(theme.fg(theme.accent));
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(text).block(block), popup);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyModifiers;

    use super::*;

    fn form(name: &str, host: &str) -> K3sForm {
        let mut form = K3sForm::default();
        for value in [name, host] {
            for c in value.chars() {
                form.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
            form.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        }
        form
    }

    /// k3s.yaml as it comes back through `ssh -t`: sudo prompt first, \r\n line ends.
    const OUTPUT: &str = "[sudo] password for additive: \r\napiVersion: v1\r\nclusters:\r\n- cluster:\r\n    certificate-authority-data: Q0E=\r\n    server: https://0.0.0.0:6443\r\n  name: default\r\nusers:\r\n- name: default\r\n  user:\r\n    client-certificate-data: Q0VSVA==\r\n    client-key-data: S0VZ\r\n";

    #[test]
    fn fetch_reads_k3s_yaml_with_sudo_over_ssh() {
        assert_eq!(helpers::shell_join(&form("teststand3", " additive@10.5.15.190 ").fetch().unwrap()), "ssh -t additive@10.5.15.190 'sudo cat /etc/rancher/k3s/k3s.yaml'");
        assert!(form("test.stand", "h").fetch().unwrap_err().starts_with("name"));
        assert!(form("", "h").fetch().unwrap_err().starts_with("name"));
        assert!(form("a", "").fetch().unwrap_err().starts_with("host"));
        assert!(form("a", "user@").fetch().unwrap_err().starts_with("host"));
    }

    #[test]
    fn commands_keep_cert_and_key_and_put_the_ip_into_the_server() {
        let commands = form("teststand3", "additive@10.5.15.190").commands(Path::new("/h/.kube/config.d/teststand3.yaml"), OUTPUT).unwrap();
        let file = "--kubeconfig=/h/.kube/config.d/teststand3.yaml";
        assert_eq!(
            commands.map(|c| helpers::shell_join(&c)),
            [
                format!("kubectl config set-cluster teststand3 --server=https://10.5.15.190:6443 --insecure-skip-tls-verify=true {file}"),
                format!("kubectl config set users.teststand3.client-certificate-data Q0VSVA== {file}"),
                format!("kubectl config set users.teststand3.client-key-data S0VZ {file}"),
                format!("kubectl config set-context teststand3 --cluster=teststand3 --user=teststand3 --namespace=default {file}"),
            ]
        );
        let error = form("a", "h").commands(Path::new("f"), "[sudo] password for additive: \r\nSorry, try again.\r\n").unwrap_err();
        assert!(error.starts_with("no server"), "{error}");
    }
}
