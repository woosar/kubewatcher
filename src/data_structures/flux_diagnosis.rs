use crate::data_structures::FluxKind;

/// A common Flux failure: what it looks like, what it means, what to run next.
/// `{kind}`, `{cli}`, `{name}` and `{ns}` in `next` are filled in for the object.
struct Rule {
    /// Matches when the message contains all of these...
    all: &'static [&'static str],
    /// ...and any of these (lowercase).
    any: &'static [&'static str],
    meaning: &'static str,
    next: &'static str,
}

const EVENTS: &str = "flux events --for {kind}/{name} {ns}";

/// First match wins, so the specific rules come before the general ones.
const RULES: &[Rule] = &[
    Rule {
        all: &["unable to clone"],
        any: &["not found", "repository not found"],
        meaning: "The repository doesn't exist at that URL, or it is private and the credentials are missing. Check spec.url and spec.secretRef.",
        next: "kubectl get {cli} {name} {ns} -o yaml",
    },
    Rule {
        all: &[],
        any: &["retries exhausted", "exceeded maximum retries"],
        meaning: "Helm gave up after its allowed attempts, and Flux won't try again by itself. Fix the cause, then reset the failure count.",
        next: "flux reconcile helmrelease {name} {ns} --reset",
    },
    Rule {
        all: &[],
        any: &["dependency"],
        meaning: "It waits for something in its spec.dependsOn to become ready. The problem is there, not here: f follows the chain.",
        next: "flux get all -A",
    },
    Rule {
        all: &["secret"],
        any: &["not found"],
        meaning: "It refers to a Secret that doesn't exist in this namespace. Create it, or fix the name in spec.secretRef.",
        next: "kubectl get secrets {ns}",
    },
    Rule {
        all: &[],
        any: &["authentication required", "authorization failed", "unauthorized", "forbidden", "access denied", "permission denied", "could not read username"],
        meaning: "The source refused the credentials. Check the Secret in spec.secretRef and that its token or deploy key is still valid.",
        next: "flux get sources all -A",
    },
    Rule {
        all: &[],
        any: &["server gave http response to https client"],
        meaning: "The registry speaks plain HTTP, but Flux connected with HTTPS. For an in-cluster registry, set spec.insecure: true on the source.",
        next: "kubectl get {cli} {name} {ns} -o yaml",
    },
    Rule {
        all: &[],
        any: &["x509", "certificate"],
        meaning: "TLS failed: the server's certificate isn't trusted. Add its CA with spec.certSecretRef on the source.",
        next: EVENTS,
    },
    Rule {
        all: &[],
        any: &["no such host", "connection refused", "i/o timeout", "network is unreachable"],
        meaning: "source-controller can't reach that address. Check the URL and that it resolves from inside the cluster.",
        next: "flux logs --level=error --kind={kind} --name={name} {ns}",
    },
    Rule {
        all: &[],
        any: &["couldn't find remote ref", "failed to checkout", "reference not found", "manifest unknown"],
        meaning: "The branch, tag or digest it should fetch doesn't exist. Compare spec.ref with what is really in the repository or registry.",
        next: "kubectl get {cli} {name} {ns} -o yaml",
    },
    Rule {
        all: &[],
        any: &["no chart version found", "no matching version", "no chart name found", "chart not found"],
        meaning: "No chart matches the name and version asked for. Check what the repository offers against spec.chart.",
        next: EVENTS,
    },
    Rule {
        all: &["source"],
        any: &["not found"],
        meaning: "The source named in its spec doesn't exist. Check the kind, name and namespace of its sourceRef; f shows what it points at.",
        next: "flux get sources all -A",
    },
    Rule {
        all: &[],
        any: &["path not found", "no such file or directory"],
        meaning: "spec.path doesn't exist in the fetched source. Check the folder name and the branch.",
        next: "kubectl get {cli} {name} {ns} -o yaml",
    },
    Rule {
        all: &[],
        any: &["decryption", "sops"],
        meaning: "Decrypting secrets failed. Check spec.decryption and the key Secret it refers to.",
        next: EVENTS,
    },
    Rule {
        all: &[],
        any: &["immutable"],
        meaning: "A manifest changes a field Kubernetes won't let you change. Delete the object or set spec.force, and Flux recreates it.",
        next: EVENTS,
    },
    Rule {
        all: &[],
        any: &["dry-run failed", "validation failed", "failed to decode", "build failed", "yaml:", "invalid"],
        meaning: "The manifests themselves are wrong, so nothing was applied. The message names the object; fix it in the source.",
        next: EVENTS,
    },
    Rule {
        all: &[],
        any: &["context deadline exceeded", "timed out", "timeout waiting", "health check failed"],
        meaning: "It was applied, but the workloads didn't get healthy in time. Look at their pods rather than at Flux.",
        next: "flux events --for {kind}/{name} {ns}",
    },
    Rule {
        all: &[],
        any: &["in progress", "progressing", "running 'install'", "running 'upgrade'", "running 'uninstall'"],
        meaning: "Flux is working on it right now. Give it a moment, or follow along in the logs.",
        next: "flux logs --kind={kind} --name={name} {ns} --tail=50",
    },
];

/// What a Flux status message means and which command to try next.
#[derive(Debug, PartialEq)]
pub struct FluxDiagnosis {
    pub meaning: &'static str,
    pub next: String,
}

impl FluxDiagnosis {
    /// Why this object is not ready, or None when the message isn't a known pattern.
    pub fn of(message: &str, kind: &FluxKind, name: &str, namespace: Option<&str>) -> Option<Self> {
        let lower = message.to_lowercase();
        let rule = RULES.iter().find(|r| r.all.iter().all(|w| lower.contains(w)) && r.any.iter().any(|w| lower.contains(w)))?;
        Some(Self { meaning: rule.meaning, next: Self::fill(rule.next, kind, name, namespace) })
    }

    /// The first thing to look at when no rule knows the message.
    pub fn events(kind: &FluxKind, name: &str, namespace: Option<&str>) -> String {
        Self::fill(EVENTS, kind, name, namespace)
    }

    fn fill(template: &str, kind: &FluxKind, name: &str, namespace: Option<&str>) -> String {
        // `flux get` spells a source `source git`, while kubectl wants the resource's full name.
        let cli = if template.starts_with("kubectl") { kind.crd.to_string() } else { kind.cli.join(" ") };
        template
            .replace("{kind}", kind.kind)
            .replace("{cli}", &cli)
            .replace("{name}", name)
            .replace("{ns}", &namespace.map(|ns| format!("-n {ns}")).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next(message: &str, resource: &str) -> Option<String> {
        FluxDiagnosis::of(message, FluxKind::by_resource(resource).unwrap(), "web", Some("apps")).map(|d| d.next)
    }

    #[test]
    fn known_failures_get_a_next_command_for_this_object() {
        assert_eq!(next("upgrade retries exhausted", "helmrelease").unwrap(), "flux reconcile helmrelease web -n apps --reset");
        assert_eq!(next("failed to get secret 'apps/token': secrets \"token\" not found", "gitrepository").unwrap(), "kubectl get secrets -n apps");
        assert_eq!(
            next("http: server gave HTTP response to HTTPS client", "ocirepository").unwrap(),
            "kubectl get ocirepositories.source.toolkit.fluxcd.io web -n apps -o yaml"
        );
        assert_eq!(next("Reconciliation in progress", "kustomization").unwrap(), "flux logs --kind=Kustomization --name=web -n apps --tail=50");
    }

    #[test]
    fn specific_rules_win_over_general_ones() {
        let meaning = |message| FluxDiagnosis::of(message, FluxKind::by_resource("kustomization").unwrap(), "w", None).unwrap().meaning;
        assert!(meaning("dependency 'apps/db' is not ready").contains("dependsOn"));
        assert!(meaning("Source 'GitRepository/x' not found").contains("source named"));
        assert!(meaning("Deployment/x dry-run failed: field is immutable").contains("won't let you"));
        assert!(meaning("dial tcp: lookup git.example.com: no such host").contains("can't reach"));
    }

    #[test]
    fn unknown_messages_fall_back_to_the_events() {
        let kind = FluxKind::by_resource("helmrelease").unwrap();
        assert_eq!(FluxDiagnosis::of("something odd", kind, "web", None), None);
        assert_eq!(FluxDiagnosis::events(kind, "web", None).trim(), "flux events --for HelmRelease/web");
    }
}
