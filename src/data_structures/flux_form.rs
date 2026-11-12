use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::data_structures::{FormAction, Theme};
use crate::helpers::{self, argv};

/// One input of the form, turned into `--flag=value`.
struct Field {
    label: &'static str,
    flag: &'static str,
    default: &'static str,
    hint: &'static str,
    required: bool,
}

const NAME_FLAG: &str = "";
const SAVE_FLAG: &str = ">";

const fn field(label: &'static str, flag: &'static str, default: &'static str, hint: &'static str, required: bool) -> Field {
    Field { label, flag, default, hint, required }
}

const NAME: Field = field("Name", NAME_FLAG, "", "lowercase letters, digits and -", true);
const NAMESPACE: Field = field("Namespace", "--namespace", "flux-system", "where the Flux object lives", true);
const SAVE: Field = field("Save to", SAVE_FLAG, "", "file in your git repo; empty only previews the YAML", false);

/// A thing `flux create` can generate.
struct Template {
    title: &'static str,
    command: &'static [&'static str],
    fixed: &'static [&'static str],
    fields: &'static [Field],
}

const TEMPLATES: &[Template] = &[
    Template {
        title: "Git source",
        command: &["source", "git"],
        fixed: &[],
        fields: &[
            NAME,
            field("URL", "--url", "https://", "the repository, e.g. https://github.com/org/repo", true),
            field("Branch", "--branch", "main", "which branch to follow", true),
            field("Interval", "--interval", "10m", "how often to look for new commits", true),
            NAMESPACE,
            SAVE,
        ],
    },
    Template {
        title: "Helm repository",
        command: &["source", "helm"],
        fixed: &[],
        fields: &[
            NAME,
            field("URL", "--url", "https://", "the chart repository, e.g. https://charts.bitnami.com/bitnami", true),
            field("Interval", "--interval", "1h", "how often to refresh its index", true),
            NAMESPACE,
            SAVE,
        ],
    },
    Template {
        title: "OCI source",
        command: &["source", "oci"],
        fixed: &[],
        fields: &[
            NAME,
            field("URL", "--url", "oci://", "the artifact, e.g. oci://registry.example.com/charts/app", true),
            field("Tag", "--tag", "latest", "which tag to follow", true),
            field("Interval", "--interval", "10m", "how often to look for a new digest", true),
            NAMESPACE,
            SAVE,
        ],
    },
    Template {
        title: "Kustomization",
        command: &["kustomization"],
        fixed: &["--prune=true"],
        fields: &[
            NAME,
            field("Source", "--source", "GitRepository/", "where the manifests come from", true),
            field("Path", "--path", "./", "folder inside the source", true),
            field("Interval", "--interval", "10m", "how often to re-apply", true),
            field("Target ns", "--target-namespace", "", "where the objects go; empty keeps theirs", false),
            NAMESPACE,
            SAVE,
        ],
    },
    Template {
        title: "HelmRelease",
        command: &["helmrelease"],
        fixed: &[],
        fields: &[
            NAME,
            field("Source", "--source", "HelmRepository/", "where the chart comes from", true),
            field("Chart", "--chart", "", "the chart's name in that source", true),
            field("Version", "--chart-version", "", "semver range; empty means the newest", false),
            field("Target ns", "--target-namespace", "", "where it installs; empty means the Namespace below", false),
            field("Interval", "--interval", "10m", "how often to check it is still as declared", true),
            NAMESPACE,
            SAVE,
        ],
    },
];

/// The "create" popup: builds a `flux create … --export` command that only prints YAML, so
/// nothing reaches the cluster until the file is committed and Flux picks it up.
pub struct FluxForm {
    template: usize,
    values: Vec<String>,
    /// 0 is the template row; n is `fields[n - 1]`.
    focus: usize,
}

impl Default for FluxForm {
    fn default() -> Self {
        let mut form = Self { template: 0, values: Vec::new(), focus: 0 };
        form.load_template(0);
        form
    }
}

impl FluxForm {
    fn load_template(&mut self, template: usize) {
        self.template = template;
        self.values = TEMPLATES[template].fields.iter().map(|f| f.default.to_string()).collect();
    }

    fn fields(&self) -> &'static [Field] {
        TEMPLATES[self.template].fields
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> FormAction {
        let rows = self.fields().len() + 1;
        match key.code {
            KeyCode::Esc => return FormAction::Cancel,
            KeyCode::Enter if self.focus == rows - 1 => return FormAction::Submit,
            KeyCode::Enter | KeyCode::Tab | KeyCode::Down => self.focus = (self.focus + 1) % rows,
            KeyCode::BackTab | KeyCode::Up => self.focus = (self.focus + rows - 1) % rows,
            KeyCode::Left | KeyCode::Right if self.focus == 0 => {
                let step = if key.code == KeyCode::Right { 1 } else { TEMPLATES.len() - 1 };
                self.load_template((self.template + step) % TEMPLATES.len());
            }
            _ if self.focus > 0 => {
                helpers::edit_input(&mut self.values[self.focus - 1], key);
            }
            _ => {}
        }
        FormAction::None
    }

    fn value(&self, flag: &str) -> &str {
        self.fields().iter().position(|f| f.flag == flag).map_or("", |i| self.values[i].trim())
    }

    pub fn title(&self) -> &'static str {
        TEMPLATES[self.template].title
    }

    /// `flux create … --export`, as far as the fields are filled in; for showing while typing.
    fn draft(&self) -> Vec<String> {
        let template = &TEMPLATES[self.template];
        let mut command = argv(&["flux", "create"]);
        command.extend(argv(template.command));
        let name = self.value(NAME_FLAG);
        command.push(if name.is_empty() { "<name>".to_string() } else { name.to_string() });
        for (field, value) in self.fields().iter().zip(&self.values) {
            let value = value.trim();
            if ![NAME_FLAG, SAVE_FLAG].contains(&field.flag) && !value.is_empty() {
                command.push(format!("{}={value}", field.flag));
            }
        }
        command.extend(argv(template.fixed));
        command.push("--export".into());
        command
    }

    /// The command to run, or what is wrong with the input.
    pub fn command(&self) -> Result<Vec<String>, String> {
        let name = self.value(NAME_FLAG);
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(format!("name '{name}': use lowercase letters, digits and - only"));
        }
        for (field, value) in self.fields().iter().zip(&self.values) {
            let value = value.trim();
            // A default like `https://` or `GitRepository/` still needs its second half.
            let unfinished = value == field.default && (value.ends_with("://") || value.ends_with("Repository/"));
            if field.required && (value.is_empty() || unfinished) {
                return Err(format!("{}: {}", field.label, field.hint));
            }
            if value.contains(char::is_whitespace) {
                return Err(format!("{}: no spaces, please", field.label));
            }
        }
        Ok(self.draft())
    }

    /// The file to write the YAML to, if any.
    pub fn save_path(&self) -> Option<&str> {
        Some(self.value(SAVE_FLAG)).filter(|p| !p.is_empty())
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let focus_style = theme.fg(theme.accent).bold();
        let type_style = if self.focus == 0 { focus_style } else { theme.fg(theme.text) };
        let mut text = vec![
            Line::from(vec![Span::styled(format!(" {:<10}", "Create"), type_style), Span::raw(format!("◂ {} ▸", self.title()))]),
            Line::styled("             ←/→ picks what to create", theme.fg(theme.muted)),
        ];
        for (i, field) in self.fields().iter().enumerate() {
            let focused = self.focus == i + 1;
            let label_style = if focused { focus_style } else { theme.fg(theme.text) };
            let mut spans = vec![Span::styled(format!(" {:<10}", field.label), label_style), Span::raw(format!("> {}", self.values[i]))];
            if focused {
                spans.push(Span::styled("█", theme.fg(theme.accent)));
            }
            text.push(Line::from(spans));
            text.push(Line::styled(format!("             {}", field.hint), theme.fg(theme.muted)));
        }
        text.push(Line::from(""));
        text.push(Line::styled(format!(" $ {}", helpers::shell_join(&self.draft())), theme.fg(theme.warning)));
        text.push(Line::styled(" only prints YAML: nothing reaches the cluster until you commit it", theme.fg(theme.muted)));
        text.push(Line::styled(" Tab/↑↓ field · Enter next / run on Save to · Esc cancel", theme.fg(theme.muted)));

        let height = text.len() as u16 + 2;
        let popup = area.centered(Constraint::Length(100.min(area.width)), Constraint::Length(height.min(area.height)));
        let block = Block::bordered().style(theme.popup()).title(" Create a Flux object ").border_style(theme.fg(theme.accent));
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new(text).wrap(ratatui::widgets::Wrap { trim: false }).block(block), popup);
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyModifiers;

    use super::*;

    fn press(form: &mut FluxForm, code: KeyCode) -> FormAction {
        form.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_in(form: &mut FluxForm, text: &str) {
        for c in text.chars() {
            press(form, KeyCode::Char(c));
        }
        press(form, KeyCode::Tab);
    }

    #[test]
    fn builds_a_git_source_command() {
        let mut form = FluxForm::default();
        assert_eq!(form.title(), "Git source");
        press(&mut form, KeyCode::Tab);
        type_in(&mut form, "repo");
        type_in(&mut form, "github.com/org/repo");
        assert_eq!(
            helpers::shell_join(&form.command().unwrap()),
            "flux create source git repo --url=https://github.com/org/repo --branch=main --interval=10m --namespace=flux-system --export"
        );
        assert_eq!(form.save_path(), None);
    }

    #[test]
    fn every_template_can_be_picked_and_requires_its_fields() {
        let mut form = FluxForm::default();
        for title in ["Helm repository", "OCI source", "Kustomization", "HelmRelease", "Git source"] {
            press(&mut form, KeyCode::Right);
            assert_eq!(form.title(), title);
        }
        press(&mut form, KeyCode::Left);
        assert_eq!(form.title(), "HelmRelease");

        assert!(form.command().unwrap_err().starts_with("name"));
        press(&mut form, KeyCode::Tab);
        type_in(&mut form, "web");
        assert!(form.command().unwrap_err().starts_with("Source"));
        type_in(&mut form, "bitnami");
        assert!(form.command().unwrap_err().starts_with("Chart"));
        type_in(&mut form, "nginx");
        type_in(&mut form, "1.x");
        type_in(&mut form, "apps");
        assert_eq!(
            helpers::shell_join(&form.command().unwrap()),
            "flux create helmrelease web --source=HelmRepository/bitnami --chart=nginx --chart-version=1.x --target-namespace=apps --interval=10m --namespace=flux-system --export"
        );

        // Interval, Namespace, then Save to, where Enter submits.
        press(&mut form, KeyCode::Tab);
        press(&mut form, KeyCode::Tab);
        for c in "clusters/web.yaml".chars() {
            press(&mut form, KeyCode::Char(c));
        }
        assert_eq!(form.save_path(), Some("clusters/web.yaml"));
        assert_eq!(press(&mut form, KeyCode::Enter), FormAction::Submit);
        assert_eq!(press(&mut form, KeyCode::Esc), FormAction::Cancel);
    }

    #[test]
    fn bad_input_is_refused() {
        let mut form = FluxForm::default();
        press(&mut form, KeyCode::Tab);
        type_in(&mut form, "Bad_Name");
        assert!(form.command().unwrap_err().starts_with("name"));

        let mut form = FluxForm::default();
        press(&mut form, KeyCode::Tab);
        type_in(&mut form, "ok");
        type_in(&mut form, "x y");
        assert!(form.command().unwrap_err().contains("no spaces"));
    }

    #[test]
    fn kustomizations_prune_what_was_removed_from_git() {
        let mut form = FluxForm::default();
        for _ in 0..3 {
            press(&mut form, KeyCode::Right);
        }
        press(&mut form, KeyCode::Tab);
        type_in(&mut form, "apps");
        type_in(&mut form, "repo");
        assert_eq!(
            helpers::shell_join(&form.command().unwrap()),
            "flux create kustomization apps --source=GitRepository/repo --path=./ --interval=10m --namespace=flux-system --prune=true --export"
        );
    }
}
