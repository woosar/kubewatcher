use crate::data_structures::FluxKind;
use crate::helpers::argv;

/// A Flux object suspended on purpose, so its live state can be changed and observed.
/// The suspension itself is on the cluster; this only remembers what to finish.
pub struct Experiment {
    kind: &'static FluxKind,
    name: String,
    namespace: Option<String>,
    /// Journal id of the suspend: later changes happened during the experiment.
    pub journal_mark: u64,
}

impl Experiment {
    pub fn new(kind: &'static FluxKind, name: String, namespace: Option<String>) -> Self {
        Self { kind, name, namespace, journal_mark: 0 }
    }

    pub fn label(&self) -> String {
        format!("{} {}", self.kind.kind, self.name)
    }

    fn command(&self, verb: &str) -> Vec<String> {
        let mut parts = vec!["flux", verb];
        parts.extend(self.kind.cli);
        parts.push(&self.name);
        let mut command = argv(&parts);
        if let Some(namespace) = &self.namespace {
            command.extend(["-n".into(), namespace.clone()]);
        }
        command
    }

    pub fn suspend_command(&self) -> Vec<String> {
        self.command("suspend")
    }

    pub fn resume_command(&self) -> Vec<String> {
        self.command("resume")
    }

    /// What resuming really does to your changes, which differs per kind.
    pub fn resume_effect(&self) -> &'static str {
        match self.kind.kind {
            "Kustomization" => "Resuming re-applies the source: live edits to fields its manifests set are overwritten. Keep a change by committing it.",
            "HelmRelease" => "Resuming only undoes live edits with spec.driftDetection on. Otherwise they stay until the chart or values change, so the cluster can differ from git without any warning.",
            _ => "Resuming makes it fetch or act again. Nothing you changed elsewhere is touched by this object.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::shell_join;

    #[test]
    fn commands_address_the_object_and_effects_differ_per_kind() {
        let release = Experiment::new(FluxKind::by_resource("helmrelease").unwrap(), "web".into(), Some("apps".into()));
        assert_eq!(release.label(), "HelmRelease web");
        assert_eq!(shell_join(&release.suspend_command()), "flux suspend helmrelease web -n apps");
        assert_eq!(shell_join(&release.resume_command()), "flux resume helmrelease web -n apps");
        assert!(release.resume_effect().contains("driftDetection"));

        let source = Experiment::new(FluxKind::by_resource("gitrepository").unwrap(), "repo".into(), None);
        assert_eq!(shell_join(&source.resume_command()), "flux resume source git repo");
        assert!(source.resume_effect().contains("fetch"));
        let kustomization = Experiment::new(FluxKind::by_resource("kustomization").unwrap(), "app".into(), None);
        assert!(kustomization.resume_effect().contains("overwritten"));
    }
}
