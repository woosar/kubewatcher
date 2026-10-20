use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Cell, Row, Table, TableState};
use serde_json::Value;

use crate::data_structures::Theme;
use crate::helpers;

const MAX_COLUMN_WIDTH: usize = 60;

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
        let constraints = widths.iter().map(|w| Constraint::Length((*w).min(MAX_COLUMN_WIDTH) as u16));

        let header = Row::new(self.headers.iter().map(|h| Cell::from(h.as_str())))
            .style(theme.header());
        let rows = visible.iter().map(|&i| {
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
            .column_spacing(2)
            .row_highlight_style(theme.selected())
            .highlight_symbol("▶ ");
        frame.render_stateful_widget(table, area, state);
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
    }
}
