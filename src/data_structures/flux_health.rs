use crate::data_structures::KubeTable;

/// One line of `flux get all -A`.
struct FluxObject {
    namespace: String,
    /// `helmrelease/web`, as flux prints it.
    name: String,
    revision: String,
    suspended: bool,
    ready: String,
    message: String,
}

impl FluxObject {
    /// Lower sorts first: what needs attention comes before what is fine.
    fn urgency(&self) -> u8 {
        match (self.ready.as_str(), self.suspended) {
            ("False", _) => 0,
            (_, true) => 1,
            ("True", false) => 3,
            _ => 2,
        }
    }
}

/// Every Flux object in the cluster and its state, from `flux get all -A`.
#[derive(Default)]
pub struct FluxHealth {
    objects: Vec<FluxObject>,
}

impl FluxHealth {
    /// The output is one tab-separated table per kind, with a blank line between them and
    /// columns that differ per kind (alerts have no REVISION).
    pub fn parse(text: &str) -> Self {
        let mut objects = Vec::new();
        let mut headers: Vec<String> = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                headers.clear();
                continue;
            }
            let cells: Vec<String> = line.split('\t').map(|c| c.trim().to_string()).collect();
            if headers.is_empty() {
                headers = cells;
                continue;
            }
            let cell = |column: &str| headers.iter().position(|h| h == column).and_then(|i| cells.get(i)).cloned().unwrap_or_default();
            objects.push(FluxObject {
                namespace: cell("NAMESPACE"),
                name: cell("NAME"),
                revision: cell("REVISION"),
                suspended: cell("SUSPENDED") == "True",
                ready: cell("READY"),
                message: cell("MESSAGE"),
            });
        }
        objects.sort_by_key(FluxObject::urgency);
        Self { objects }
    }

    /// The objects as a table, most urgent first; `problems_only` leaves out the healthy ones.
    pub fn table(&self, problems_only: bool) -> KubeTable {
        let headers = ["NAMESPACE", "NAME", "READY", "SUSPENDED", "REVISION", "MESSAGE"].map(String::from).to_vec();
        let rows = self
            .objects
            .iter()
            .filter(|o| !problems_only || o.urgency() < 3)
            .map(|o| {
                let suspended = if o.suspended { "True" } else { "False" };
                vec![o.namespace.clone(), o.name.clone(), o.ready.clone(), suspended.to_string(), o.revision.clone(), o.message.clone()]
            })
            .collect();
        KubeTable { headers, rows }
    }

    pub fn suspended(&self) -> Vec<&str> {
        self.objects.iter().filter(|o| o.suspended).map(|o| o.name.as_str()).collect()
    }

    pub fn failing(&self) -> Vec<&str> {
        self.objects.iter().filter(|o| o.ready == "False").map(|o| o.name.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape printed by a real cluster, trailing tabs and padding included.
    const ALL: &str = "NAMESPACE  \tNAME                 \tREVISION      \tSUSPENDED\tREADY\tMESSAGE    \t\n\
flux-system\tocirepository/homelab\t0.1.0@sha256:a\tFalse    \tTrue \tstored artifact\t\n\
\n\
NAMESPACE  \tNAME               \tREVISION\tSUSPENDED\tREADY\tMESSAGE \t\n\
flux-system\thelmrelease/paused \t1.0     \tTrue     \tTrue \tok      \t\n\
apps       \thelmrelease/broken \t1.0     \tFalse    \tFalse\tretries exhausted\t\n\
apps       \thelmrelease/busy   \t1.0     \tFalse    \tUnknown\tin progress\t\n\
\n\
NAMESPACE  \tNAME        \tSUSPENDED\tREADY\tMESSAGE\t\n\
apps       \talert/notify\tFalse    \tTrue \tInitialized\t\n";

    #[test]
    fn parses_every_table_and_puts_the_urgent_ones_first() {
        let health = FluxHealth::parse(ALL);
        assert_eq!(health.suspended(), ["helmrelease/paused"]);
        assert_eq!(health.failing(), ["helmrelease/broken"]);

        let table = health.table(false);
        let names: Vec<_> = table.rows.iter().map(|r| table.cell(r, "NAME").unwrap()).collect();
        assert_eq!(names, ["helmrelease/broken", "helmrelease/paused", "helmrelease/busy", "ocirepository/homelab", "alert/notify"]);
        assert_eq!(table.cell(&table.rows[0], "MESSAGE"), Some("retries exhausted"));
        assert_eq!(table.cell(&table.rows[4], "REVISION"), None);

        let problems = health.table(true);
        assert_eq!(problems.rows.len(), 3);
    }

    #[test]
    fn nothing_listed_means_nothing_to_report() {
        let health = FluxHealth::parse("");
        assert!(health.suspended().is_empty() && health.failing().is_empty());
        assert!(health.table(false).rows.is_empty());
    }
}
