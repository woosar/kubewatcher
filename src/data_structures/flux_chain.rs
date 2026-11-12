use serde_json::Value;

/// Asks only for the references: a HelmRelease's full `-o yaml` holds its values, which can
/// contain secrets and would pass through the journal.
pub const CHAIN_JSONPATH: &str = r#"jsonpath={.spec.sourceRef}{"\n"}{.spec.chart.spec.sourceRef}{"\n"}{.spec.chartRef}{"\n"}{.spec.dependsOn}{"\n"}"#;

/// Another Flux object that this one needs.
pub struct FluxLink {
    pub role: &'static str,
    pub kind: String,
    pub name: String,
    pub namespace: String,
}

/// What a Flux object depends on: where its content comes from and what must be ready first.
pub struct FluxChain {
    /// `HelmRelease homelab`
    pub owner: String,
    pub links: Vec<FluxLink>,
}

impl FluxChain {
    /// Reads the output of `CHAIN_JSONPATH`, one line per field in the order asked for. References
    /// without a namespace point into the object's own, and `dependsOn` to its own kind.
    pub fn parse(text: &str, owner_kind: &str, owner_name: &str, owner_namespace: &str) -> Self {
        let mut links = Vec::new();
        let link = |role, value: &Value, default_kind: &str| {
            Some(FluxLink {
                role,
                kind: value["kind"].as_str().unwrap_or(default_kind).to_string(),
                name: value["name"].as_str()?.to_string(),
                namespace: value["namespace"].as_str().unwrap_or(owner_namespace).to_string(),
            })
        };
        for (line, role) in text.lines().zip(["source", "chart source", "chart", "depends on"]) {
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else { continue };
            match &value {
                Value::Array(items) => links.extend(items.iter().filter_map(|item| link(role, item, owner_kind))),
                single => links.extend(link(role, single, owner_kind)),
            }
        }
        Self { owner: format!("{owner_kind} {owner_name}"), links }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_chart_references_and_dependencies() {
        // Exactly what kubectl printed for a HelmRelease using chartRef.
        let chain = FluxChain::parse("\n\n{\"kind\":\"OCIRepository\",\"name\":\"homelab\"}\n\n", "HelmRelease", "homelab", "flux-system");
        assert_eq!(chain.owner, "HelmRelease homelab");
        assert_eq!(chain.links.len(), 1);
        let link = &chain.links[0];
        assert_eq!((link.role, link.kind.as_str(), link.name.as_str(), link.namespace.as_str()), ("chart", "OCIRepository", "homelab", "flux-system"));

        let text = "{\"kind\":\"GitRepository\",\"name\":\"repo\",\"namespace\":\"flux-system\"}\n\n\n[{\"name\":\"db\"},{\"name\":\"cache\",\"namespace\":\"infra\"}]\n";
        let chain = FluxChain::parse(text, "Kustomization", "web", "apps");
        let links: Vec<_> = chain.links.iter().map(|l| format!("{} {}/{}/{}", l.role, l.kind, l.namespace, l.name)).collect();
        assert_eq!(links, ["source GitRepository/flux-system/repo", "depends on Kustomization/apps/db", "depends on Kustomization/infra/cache"]);
    }

    #[test]
    fn a_source_has_nothing_upstream() {
        assert!(FluxChain::parse("\n\n\n\n", "GitRepository", "repo", "flux-system").links.is_empty());
        assert!(FluxChain::parse("not json\n", "GitRepository", "repo", "flux-system").links.is_empty());
    }
}
