use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Cell, Row, Table, TableState};
use serde_json::Value;

use crate::data_structures::Theme;
use crate::helpers;

const MAX_COLUMN_WIDTH: usize = 60;
const COLUMN_SPACING: u16 = 2;
/// The `▶ ` in front of the selected row.
const HIGHLIGHT_WIDTH: u16 = 2;

/// Tabular command output (kubectl's aligned columns or helm's JSON) as headers + rows.
#[derive(Default)]
pub struct KubeTable {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl KubeTable {
    /// Parses kubectl's column-aligned output. Column boundaries come from the header
    /// line: a column starts wherever a word follows two or more spaces, so headers like
    /// "LAST SEEN" or "NOMINATED NODE" stay one column and empty cells are preserved.
    pub fn parse(text: &str) -> Self {
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let Some(header) = lines.next() else {
            return Self::default();
        };
        // The flux CLI separates columns with tabs instead of aligning them.
        if header.contains('\t') {
            let split = |line: &str| line.split('\t').map(|c| c.trim().to_string()).collect::<Vec<_>>();
            let headers = split(header);
            let rows = lines.map(split).collect();
            return Self { headers, rows };
        }
        let header: Vec<char> = header.chars().collect();
        let starts: Vec<usize> = (0..header.len())
            .filter(|&i| header[i] != ' ' && (i == 0 || (i >= 2 && header[i - 1] == ' ' && header[i - 2] == ' ')))
            .collect();

        let slice = |chars: &[char], col: usize| -> String {
            let start = starts[col].min(chars.len());
            let end = starts.get(col + 1).map_or(chars.len(), |&e| e.min(chars.len()));
            chars[start..end].iter().collect::<String>().trim().to_string()
        };

        let headers = (0..starts.len()).map(|c| slice(&header, c)).collect();
        let rows = lines
            .map(|line| {
                let chars: Vec<char> = line.chars().collect();
                (0..starts.len()).map(|c| slice(&chars, c)).collect()
            })
            .collect();
        Self { headers, rows }
    }

    /// Builds a table from `helm list -o json`.
    pub fn from_helm_json(text: &str) -> Result<Self, String> {
        const COLUMNS: [(&str, &str); 7] = [
            ("NAME", "name"),
            ("NAMESPACE", "namespace"),
            ("REVISION", "revision"),
            ("UPDATED", "updated"),
            ("STATUS", "status"),
            ("CHART", "chart"),
            ("APP VERSION", "app_version"),
        ];
        let releases: Vec<Value> =
            serde_json::from_str(text.trim()).map_err(|e| format!("unexpected helm output: {e}"))?;
        let rows = releases
            .iter()
            .map(|release| {
                COLUMNS
                    .iter()
                    .map(|(header, key)| {
                        let text = match &release[*key] {
                            Value::String(s) => s.clone(),
                            Value::Null => String::new(),
                            other => other.to_string(),
                        };
                        // "2024-05-01 10:11:12.123456 +0000 UTC" -> "2024-05-01 10:11:12"
                        if *header == "UPDATED" { text.chars().take(19).collect() } else { text }
                    })
                    .collect()
            })
            .collect();
        Ok(Self {
            headers: COLUMNS.iter().map(|(h, _)| h.to_string()).collect(),
            rows,
        })
    }

    pub fn column(&self, name: &str) -> Option<usize> {
        self.headers.iter().position(|h| h == name)
    }

    pub fn cell<'a>(&self, row: &'a [String], column: &str) -> Option<&'a str> {
        self.column(column)
            .and_then(|i| row.get(i))
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    }

    /// Indices of rows matching the filter, in display order.
    pub fn visible_rows(&self, filter: &str) -> Vec<usize> {
        (0..self.rows.len())
            .filter(|&i| helpers::matches_filter(self.rows[i].iter().map(String::as_str), filter))
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        frame: &mut Frame,
        area: Rect,
        visible: &[usize],
        state: &mut TableState,
        block: Block,
        theme: &Theme,
        row_style: impl Fn(&[String]) -> Option<Style>,
    ) {
        let styled_columns: Vec<Option<&str>> = self
            .headers
            .iter()
            .map(|h| matches!(h.as_str(), "STATUS" | "READY" | "SUSPENDED").then_some(h.as_str()))
            .collect();

        let mut widths: Vec<usize> = self.headers.iter().map(|h| h.chars().count()).collect();
        for &i in visible {
            for (width, cell) in widths.iter_mut().zip(&self.rows[i]) {
                *width = (*width).max(cell.chars().count());
            }
        }
        // When the columns don't all fit, those on the right give way, one after the other. Left to
        // the layout, every column shrinks, which cuts the NAME you act on. The last column takes
        // whatever is left, as it is usually free text (flux's MESSAGE).
        let mut left = block.inner(area).width.saturating_sub(HIGHLIGHT_WIDTH) as usize;
        let last = widths.len().saturating_sub(1);
        let constraints: Vec<Constraint> = widths
            .iter()
            .enumerate()
            .map(|(i, width)| {
                let width = (*width).min(MAX_COLUMN_WIDTH).min(left);
                left = left.saturating_sub(width + COLUMN_SPACING as usize);
                if i == last { Constraint::Fill(1) } else { Constraint::Length(width as u16) }
            })
            .collect();

        // Only the rows that fit are built: with thousands of pods, building every row on
        // every frame made scrolling lag. The scroll offset is kept in `state` as ratatui would.
        let height = block.inner(area).height.saturating_sub(1) as usize;
        let selected = state.selected().map(|s| s.min(visible.len().saturating_sub(1)));
        let mut offset = state.offset().min(visible.len().saturating_sub(height));
        if let Some(selected) = selected {
            if selected < offset {
                offset = selected;
            } else if height > 0 && selected >= offset + height {
                offset = selected + 1 - height;
            }
        }
        let window = &visible[offset..(offset + height).min(visible.len())];
        let mut window_state = TableState::default().with_selected(selected.map(|s| s - offset));

        let header = Row::new(self.headers.iter().map(|h| Cell::from(h.as_str())))
            .style(theme.header());
        let rows = window.iter().map(|&i| {
            let row = &self.rows[i];
            let cells = row.iter().enumerate().map(|(c, text)| {
                let cell = Cell::from(text.as_str());
                match styled_columns.get(c).copied().flatten() {
                    Some(column) => cell.style(Self::cell_style(theme, column, text)),
                    None => cell,
                }
            });
            let mut table_row = Row::new(cells);
            if let Some(style) = row_style(row) {
                table_row = table_row.style(style);
            }
            table_row
        });

        let table = Table::new(rows, constraints)
            .header(header)
            .block(block)
            .column_spacing(COLUMN_SPACING)
            .row_highlight_style(theme.selected())
            .highlight_symbol("▶ ");
        frame.render_stateful_widget(table, area, &mut window_state);
        *state.offset_mut() = offset;
    }

    /// Colors well-known states: pod/helm STATUS, flux READY ("True"/"False") and SUSPENDED.
    fn cell_style(theme: &Theme, column: &str, text: &str) -> Style {
        match (column, text) {
            ("READY", "True") => theme.fg(theme.success),
            ("READY", "False") => theme.fg(theme.danger).add_modifier(Modifier::BOLD),
            ("READY", "Unknown") => theme.fg(theme.warning),
            ("SUSPENDED", "True") => theme.fg(theme.warning).add_modifier(Modifier::BOLD),
            ("STATUS", status) => Self::status_style(theme, status),
            _ => Style::new(),
        }
    }

    /// Colors well-known pod / helm release states.
    fn status_style(theme: &Theme, status: &str) -> Style {
        let lower = status.to_lowercase();
        let bad = ["error", "crash", "backoff", "fail", "evicted", "oomkilled", "invalid", "unknown", "notready"];
        let pending = ["pending", "creating", "init", "terminating", "progress", "uninstalling"];
        if bad.iter().any(|b| lower.contains(b)) {
            theme.fg(theme.danger).add_modifier(Modifier::BOLD)
        } else if pending.iter().any(|p| lower.contains(p)) {
            theme.fg(theme.warning)
        } else if ["running", "deployed", "ready", "active", "bound"].contains(&lower.as_str()) {
            theme.fg(theme.success)
        } else if ["completed", "succeeded", "superseded"].contains(&lower.as_str()) {
            theme.fg(theme.muted)
        } else {
            Style::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_word_headers_and_empty_cells() {
        let text = "\
CURRENT   NAME        CLUSTER     AUTHINFO    NAMESPACE
*         prod        prod-eks    admin       shop
          staging     stg-eks     admin       
";
        let table = KubeTable::parse(text);
        assert_eq!(table.headers, ["CURRENT", "NAME", "CLUSTER", "AUTHINFO", "NAMESPACE"]);
        assert_eq!(table.cell(&table.rows[0], "CURRENT"), Some("*"));
        assert_eq!(table.cell(&table.rows[1], "CURRENT"), None);
        assert_eq!(table.cell(&table.rows[1], "NAME"), Some("staging"));

        let events = KubeTable::parse("LAST SEEN   TYPE      REASON\n5m          Warning   BackOff\n");
        assert_eq!(events.headers, ["LAST SEEN", "TYPE", "REASON"]);
        assert_eq!(events.rows[0], ["5m", "Warning", "BackOff"]);
    }

    #[test]
    fn parses_tab_separated_flux_output() {
        let text = "NAMESPACE  \tNAME       \tREVISION\tSUSPENDED\tREADY\tMESSAGE\nflux-system\tapps\tmain@sha1:abc\tFalse\tTrue\tApplied revision: main\n";
        let table = KubeTable::parse(text);
        assert_eq!(table.headers, ["NAMESPACE", "NAME", "REVISION", "SUSPENDED", "READY", "MESSAGE"]);
        assert_eq!(table.cell(&table.rows[0], "MESSAGE"), Some("Applied revision: main"));
    }

    #[test]
    fn parses_helm_json() {
        let json = r#"[{"name":"web","namespace":"shop","revision":"3","updated":"2024-05-01 10:11:12.123 +0000 UTC","status":"deployed","chart":"web-1.2.0","app_version":"1.2"}]"#;
        let table = KubeTable::from_helm_json(json).unwrap();
        assert_eq!(table.cell(&table.rows[0], "UPDATED"), Some("2024-05-01 10:11:12"));
        assert_eq!(table.cell(&table.rows[0], "REVISION"), Some("3"));

        let mixed = KubeTable::from_helm_json(r#"[{"name":"db","revision":7,"app_version":null}]"#).unwrap();
        assert_eq!(mixed.cell(&mixed.rows[0], "REVISION"), Some("7"));
        assert_eq!(mixed.cell(&mixed.rows[0], "APP VERSION"), None);
        assert!(KubeTable::from_helm_json("Error: not json").is_err_and(|e| e.starts_with("unexpected helm output")));
    }

    #[test]
    fn empty_output_and_filtering() {
        let empty = KubeTable::parse("\n  \n");
        assert!(empty.headers.is_empty() && empty.rows.is_empty());

        let table = KubeTable::parse("NAME   STATUS\nweb    Running\ndb     Pending\n");
        assert_eq!(table.visible_rows("pend"), [1]);
        assert_eq!(table.visible_rows(""), [0, 1]);
        assert_eq!(table.column("MISSING"), None);
    }

    #[test]
    fn colors_well_known_states() {
        let theme = Theme::default();
        let danger = theme.fg(theme.danger).add_modifier(Modifier::BOLD);
        assert_eq!(KubeTable::cell_style(&theme, "READY", "True"), theme.fg(theme.success));
        assert_eq!(KubeTable::cell_style(&theme, "READY", "False"), danger);
        assert_eq!(KubeTable::cell_style(&theme, "READY", "Unknown"), theme.fg(theme.warning));
        assert_eq!(KubeTable::cell_style(&theme, "READY", "1/1"), Style::new());
        assert_eq!(
            KubeTable::cell_style(&theme, "SUSPENDED", "True"),
            theme.fg(theme.warning).add_modifier(Modifier::BOLD)
        );
        assert_eq!(KubeTable::cell_style(&theme, "STATUS", "CrashLoopBackOff"), danger);
        assert_eq!(KubeTable::cell_style(&theme, "STATUS", "ContainerCreating"), theme.fg(theme.warning));
        assert_eq!(KubeTable::cell_style(&theme, "STATUS", "deployed"), theme.fg(theme.success));
        assert_eq!(KubeTable::cell_style(&theme, "STATUS", "Completed"), theme.fg(theme.muted));
        assert_eq!(KubeTable::cell_style(&theme, "STATUS", "Whatever"), Style::new());
    }

    #[test]
    fn renders_rows_with_styles() {
        let table = KubeTable::parse("NAME   STATUS    READY\nweb    Running   True\ndb     Error     False\n");
        let theme = Theme::default();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 6)).unwrap();
        let mut state = TableState::default().with_selected(Some(0));
        terminal
            .draw(|frame| {
                let highlight = |row: &[String]| (row[0] == "db").then(Style::new);
                table.render(frame, frame.area(), &[0, 1], &mut state, theme.block(), &theme, highlight);
            })
            .unwrap();
        let screen = terminal.backend().to_string();
        assert!(screen.contains("web") && screen.contains("Error"));
    }
}
