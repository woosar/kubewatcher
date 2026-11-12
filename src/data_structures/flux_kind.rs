/// A Flux object type: how each flux CLI subcommand spells it, and what it is for.
pub struct FluxKind {
    pub label: &'static str,
    /// `flux get <get...>`
    pub get: &'static [&'static str],
    /// `flux suspend|resume|reconcile <cli...> NAME`
    pub cli: &'static [&'static str],
    /// Full CRD name for kubectl describe / get -o yaml.
    pub crd: &'static str,
    /// Kind as used by `flux events --for` and `flux logs --kind`.
    pub kind: &'static str,
    /// The controller that handles this kind; a cluster only has the ones it installed.
    pub controller: &'static str,
    /// `flux reconcile` exists for this kind.
    pub reconcile: bool,
    /// reconcile supports --with-source (fetch git/chart first).
    pub with_source: bool,
    /// `flux tree` is supported.
    pub tree: bool,
    /// What this kind is, in a sentence or two, for the details panel.
    pub card: &'static str,
}

const SOURCE: FluxKind = FluxKind {
    label: "",
    get: &[],
    cli: &[],
    crd: "",
    kind: "",
    controller: "source-controller",
    reconcile: true,
    with_source: false,
    tree: false,
    card: "",
};

/// The first entry is the overview over everything; the others are one kind each.
pub const FLUX_KINDS: &[FluxKind] = &[
    FluxKind {
        label: "overview",
        get: &["all"],
        controller: "",
        reconcile: false,
        card: "Everything Flux manages, most urgent first. Flux pulls a source, then a Kustomization or HelmRelease applies it.",
        ..SOURCE
    },
    FluxKind {
        label: "kustomizations",
        get: &["kustomizations"],
        cli: &["kustomization"],
        crd: "kustomizations.kustomize.toolkit.fluxcd.io",
        kind: "Kustomization",
        controller: "kustomize-controller",
        with_source: true,
        tree: true,
        card: "Applies a folder from a source to the cluster and keeps it that way: live edits to fields it sets get overwritten on every interval.",
        ..SOURCE
    },
    FluxKind {
        label: "helmreleases",
        get: &["helmreleases"],
        cli: &["helmrelease"],
        crd: "helmreleases.helm.toolkit.fluxcd.io",
        kind: "HelmRelease",
        controller: "helm-controller",
        with_source: true,
        card: "Installs and upgrades a Helm chart from a source. Live edits are only undone with spec.driftDetection on; otherwise they stay until the chart or values change.",
        ..SOURCE
    },
    FluxKind {
        label: "git sources",
        get: &["sources", "git"],
        cli: &["source", "git"],
        crd: "gitrepositories.source.toolkit.fluxcd.io",
        kind: "GitRepository",
        card: "Fetches a git repository on an interval and stores it as an artifact. It applies nothing itself; Kustomizations and HelmReleases consume it.",
        ..SOURCE
    },
    FluxKind {
        label: "helm repositories",
        get: &["sources", "helm"],
        cli: &["source", "helm"],
        crd: "helmrepositories.source.toolkit.fluxcd.io",
        kind: "HelmRepository",
        card: "Points at a Helm chart repository. HelmCharts fetch their charts from it.",
        ..SOURCE
    },
    FluxKind {
        label: "oci sources",
        get: &["sources", "oci"],
        cli: &["source", "oci"],
        crd: "ocirepositories.source.toolkit.fluxcd.io",
        kind: "OCIRepository",
        card: "Fetches an artifact (manifests or a chart) from a container registry. A new tag or digest here is what triggers an upgrade downstream.",
        ..SOURCE
    },
    FluxKind {
        label: "helm charts",
        get: &["sources", "chart"],
        cli: &["source", "chart"],
        crd: "helmcharts.source.toolkit.fluxcd.io",
        kind: "HelmChart",
        card: "A packaged chart fetched from a source. Usually generated from a HelmRelease, so change the HelmRelease rather than this.",
        ..SOURCE
    },
    FluxKind {
        label: "buckets",
        get: &["sources", "bucket"],
        cli: &["source", "bucket"],
        crd: "buckets.source.toolkit.fluxcd.io",
        kind: "Bucket",
        card: "Fetches files from S3-compatible storage. Works like a git source, without git.",
        ..SOURCE
    },
    FluxKind {
        label: "alerts",
        get: &["alerts"],
        cli: &["alert"],
        crd: "alerts.notification.toolkit.fluxcd.io",
        kind: "Alert",
        controller: "notification-controller",
        reconcile: false,
        card: "Chooses which Flux events are sent where, through a Provider.",
        ..SOURCE
    },
    FluxKind {
        label: "alert providers",
        get: &["alert-providers"],
        cli: &["alert-provider"],
        crd: "providers.notification.toolkit.fluxcd.io",
        kind: "Provider",
        controller: "notification-controller",
        reconcile: false,
        card: "Where notifications go: Slack, Teams, a webhook, and so on.",
        ..SOURCE
    },
    FluxKind {
        label: "receivers",
        get: &["receivers"],
        cli: &["receiver"],
        crd: "receivers.notification.toolkit.fluxcd.io",
        kind: "Receiver",
        controller: "notification-controller",
        card: "A webhook that lets a push reconcile a source right away, instead of waiting for its interval.",
        ..SOURCE
    },
    FluxKind {
        label: "image repositories",
        get: &["images", "repository"],
        cli: &["image", "repository"],
        crd: "imagerepositories.image.toolkit.fluxcd.io",
        kind: "ImageRepository",
        controller: "image-reflector-controller",
        card: "Scans a container registry for new image tags.",
        ..SOURCE
    },
    FluxKind {
        label: "image policies",
        get: &["images", "policy"],
        cli: &["image", "policy"],
        crd: "imagepolicies.image.toolkit.fluxcd.io",
        kind: "ImagePolicy",
        controller: "image-reflector-controller",
        card: "Picks the newest allowed tag from what an ImageRepository found, e.g. by semver range.",
        ..SOURCE
    },
    FluxKind {
        label: "image updates",
        get: &["images", "update"],
        cli: &["image", "update"],
        crd: "imageupdateautomations.image.toolkit.fluxcd.io",
        kind: "ImageUpdateAutomation",
        controller: "image-automation-controller",
        card: "Commits the tag an ImagePolicy picked back to git, so Flux itself writes to your repository.",
        ..SOURCE
    },
];

impl FluxKind {
    pub fn is_overview(&self) -> bool {
        self.get == ["all"]
    }

    /// The kind whose name `flux get all` prints in front of the slash (`helmrelease/web`).
    pub fn by_resource(prefix: &str) -> Option<&'static FluxKind> {
        FLUX_KINDS.iter().find(|k| !k.is_overview() && k.kind.eq_ignore_ascii_case(prefix))
    }

    /// Position in `FLUX_KINDS` of a kind named in a `sourceRef` or `chartRef`.
    pub fn index_of(kind: &str) -> Option<usize> {
        FLUX_KINDS.iter().position(|k| k.kind == kind)
    }

    /// `flux get` exits 1 for two situations that aren't failures: there are no such objects,
    /// or this cluster doesn't run the controller that defines them.
    pub fn list_result(&self, result: Result<String, String>) -> Result<String, String> {
        match result {
            Err(e) if e.contains("objects found") => Ok(String::new()),
            Err(e) if e.contains("complete list of server APIs") && !self.controller.is_empty() => Err(format!(
                "This cluster doesn't run the {}, so it has no {}.\n\nFlux is modular: `flux check` lists the controllers installed here.",
                self.controller, self.label
            )),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_found_by_the_names_flux_prints() {
        assert_eq!(FluxKind::by_resource("helmrelease").unwrap().kind, "HelmRelease");
        assert_eq!(FluxKind::by_resource("ocirepository").unwrap().kind, "OCIRepository");
        assert_eq!(FluxKind::by_resource("provider").unwrap().label, "alert providers");
        assert!(FluxKind::by_resource("").is_none() && FluxKind::by_resource("pod").is_none());
        assert_eq!(FLUX_KINDS[FluxKind::index_of("OCIRepository").unwrap()].label, "oci sources");
        assert!(FLUX_KINDS[0].is_overview());
    }

    #[test]
    fn list_results_tell_failures_from_empty_and_not_installed() {
        let alerts = FluxKind::by_resource("alert").unwrap();
        let none = Err("✗ no Alert objects found in any namespace".to_string());
        assert_eq!(alerts.list_result(none), Ok(String::new()));
        let missing = Err("✗ unable to retrieve the complete list of server APIs: notification.toolkit.fluxcd.io/v1: no matches".to_string());
        assert!(alerts.list_result(missing).unwrap_err().contains("doesn't run the notification-controller"));
        assert_eq!(alerts.list_result(Err("boom".into())), Err("boom".to_string()));
        assert_eq!(alerts.list_result(Ok("x".into())), Ok("x".to_string()));
    }
}
