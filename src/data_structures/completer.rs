use crate::helpers;

const MAX_SUGGESTIONS: usize = 50;

/// Canonical kind (as printed by `-o name`) and the spellings kubectl accepts for it.
const KINDS: &[(&str, &[&str])] = &[
    ("pod", &["pods", "pod", "po"]),
    ("service", &["services", "service", "svc"]),
    ("deployment", &["deployments", "deployment", "deploy"]),
    ("statefulset", &["statefulsets", "statefulset", "sts"]),
    ("daemonset", &["daemonsets", "daemonset", "ds"]),
    ("replicaset", &["replicasets", "replicaset", "rs"]),
    ("configmap", &["configmaps", "configmap", "cm"]),
    ("secret", &["secrets", "secret"]),
    ("ingress", &["ingresses", "ingress", "ing"]),
    ("job", &["jobs", "job"]),
    ("cronjob", &["cronjobs", "cronjob", "cj"]),
    ("persistentvolumeclaim", &["persistentvolumeclaims", "persistentvolumeclaim", "pvc"]),
    ("horizontalpodautoscaler", &["horizontalpodautoscalers", "hpa"]),
    ("node", &["nodes", "node", "no"]),
    ("namespace", &["namespaces", "namespace", "ns"]),
    ("event", &["events", "event", "ev"]),
];

/// Kinds fetched for name completion (one `kubectl get a,b,c -o name` call).
pub const COMPLETION_KINDS: &str =
    "pods,services,deployments,statefulsets,daemonsets,configmaps,secrets,ingresses,jobs,cronjobs,persistentvolumeclaims,nodes";

const KUBECTL_VERBS: &[&str] = &[
    "get", "describe", "logs", "exec", "edit", "delete", "apply", "diff", "rollout", "scale", "port-forward",
    "top", "config", "explain", "label", "annotate", "cp", "debug", "run", "create", "set", "events",
    "api-resources", "cluster-info", "auth", "cordon", "uncordon", "drain", "version",
];
const KUBECTL_CONFIG: &[&str] = &[
    "use-context", "get-contexts", "current-context", "set-context", "view", "rename-context", "delete-context",
];
const KUBECTL_ROLLOUT: &[&str] = &["restart", "status", "history", "undo", "pause", "resume"];
const KUBECTL_FLAGS: &[&str] = &[
    "-n", "-A", "-o", "-l", "-f", "-w", "-c", "--tail=100", "--previous", "--all-containers", "--prefix",
    "--since=10m", "--sort-by=.lastTimestamp", "--show-labels", "--dry-run=client", "--replicas=",
];
const HELM_VERBS: &[&str] = &[
    "list", "status", "history", "rollback", "uninstall", "upgrade", "install", "get", "repo", "search", "show",
    "template", "lint", "pull", "diff", "dependency",
];
const HELM_GET: &[&str] = &["values", "manifest", "notes", "all", "hooks"];
const HELM_REPO: &[&str] = &["add", "update", "list", "remove"];
const FLUX_VERBS: &[&str] = &[
    "get", "reconcile", "suspend", "resume", "logs", "events", "tree", "trace", "debug", "check", "diff", "create", "export", "stats",
];
const FLUX_KINDS: &[&str] = &["kustomization", "helmrelease", "source", "image", "alert", "alert-provider", "receiver", "all"];
const HELM_FLAGS: &[&str] = &[
    "-n", "-A", "-f", "--set", "--reuse-values", "--install", "--create-namespace", "--atomic", "--dry-run=server",
    "--debug", "--all", "--version",
];
/// Flags whose next word is a value, not a positional argument.
const VALUE_FLAGS: &[&str] = &["-n", "--namespace", "-o", "-l", "-c", "--context", "--selector", "--container", "-f", "--set"];

/// An in-progress Tab cycle: the word being completed starts at `start`.
struct Cycle {
    start: usize,
    candidates: Vec<String>,
    index: usize,
}

/// Shell-style completion for the command prompt, backed by live cluster data.
#[derive(Default)]
pub struct Completer {
    /// (canonical kind, name)
    resources: Vec<(String, String)>,
    pub namespaces: Vec<String>,
    pub releases: Vec<String>,
    pub contexts: Vec<String>,
    /// Names of the flux objects currently listed in the Flux tab.
    pub flux_names: Vec<String>,
    cycle: Option<Cycle>,
}

impl Completer {
    /// Loads `kubectl get ... -o name` output ("deployment.apps/web" -> ("deployment", "web")).
    pub fn set_resources(&mut self, text: &str) {
        self.resources = text
            .lines()
            .filter_map(|line| line.trim().split_once('/'))
            .map(|(kind, name)| (kind.split('.').next().unwrap_or(kind).to_string(), name.to_string()))
            .collect();
    }

    pub fn reset(&mut self) {
        self.cycle = None;
    }

    /// Candidates shown under the prompt, and which one Tab would pick.
    pub fn suggestions(&self, input: &str) -> (Vec<String>, Option<usize>) {
        match &self.cycle {
            Some(cycle) => (cycle.candidates.clone(), Some(cycle.index)),
            None => (self.complete(input).1, None),
        }
    }

    /// Tab / Shift-Tab: inserts the next (or previous) candidate for the word under the cursor.
    pub fn tab(&mut self, input: &mut String, backwards: bool) {
        match self.cycle.as_mut() {
            Some(cycle) => {
                let len = cycle.candidates.len();
                cycle.index = if backwards { (cycle.index + len - 1) % len } else { (cycle.index + 1) % len };
            }
            None => {
                let (start, candidates) = self.complete(input);
                if candidates.is_empty() {
                    return;
                }
                if candidates.len() == 1 {
                    input.truncate(start);
                    input.push_str(&candidates[0]);
                    input.push(' ');
                    return;
                }
                let index = if backwards { candidates.len() - 1 } else { 0 };
                self.cycle = Some(Cycle { start, candidates, index });
            }
        }
        if let Some(cycle) = &self.cycle {
            input.truncate(cycle.start);
            input.push_str(&cycle.candidates[cycle.index]);
        }
    }

    /// Returns the byte offset of the word being completed and the matching candidates.
    fn complete(&self, input: &str) -> (usize, Vec<String>) {
        let start = input.rfind(' ').map_or(0, |i| i + 1);
        let current = &input[start..];
        let expanded = helpers::expand_alias(&input[..start]);
        let mut words: Vec<&str> = expanded.split_whitespace().collect();

        // `watch [-n N] cmd` completes like `cmd`
        if words.first() == Some(&"watch") {
            words.remove(0);
            if words.first() == Some(&"-n") {
                words.drain(..words.len().min(2));
            }
        }

        let candidates: Vec<String> = match words.first() {
            None => {
                let mut first = vec!["kubectl".to_string(), "helm".to_string(), "flux".to_string(), "watch".to_string()];
                first.extend(helpers::ALIASES.iter().map(|(alias, _)| alias.to_string()));
                first
            }
            Some(&"kubectl") => self.complete_kubectl(&words[1..], current),
            Some(&"helm") => self.complete_helm(&words[1..], current),
            Some(&"flux") => match Self::positional(&words[1..]).as_slice() {
                [] => Self::owned(FLUX_VERBS),
                [_] => Self::owned(FLUX_KINDS),
                [_, "source"] => Self::owned(&["git", "helm", "oci", "bucket", "chart"]),
                _ => self.flux_names.clone(),
            },
            Some(_) => Vec::new(),
        };
        (start, Self::rank(candidates, current))
    }

    fn complete_kubectl(&self, words: &[&str], current: &str) -> Vec<String> {
        if let Some(values) = self.flag_values(words) {
            return values;
        }
        if current.starts_with('-') {
            return Self::owned(KUBECTL_FLAGS);
        }
        let positional = Self::positional(words);
        let Some(&verb) = positional.first() else {
            return Self::owned(KUBECTL_VERBS);
        };
        // kind/name form: "svc/my-ser" -> "svc/my-service"
        if let Some((alias, _)) = current.split_once('/') {
            return Self::canonical_kind(alias)
                .map(|kind| self.names_of(kind).map(|name| format!("{alias}/{name}")).collect())
                .unwrap_or_default();
        }
        let args = &positional[1..];
        match verb {
            "config" => match args.first() {
                None => Self::owned(KUBECTL_CONFIG),
                Some(&("use-context" | "delete-context" | "rename-context")) if args.len() == 1 => self.contexts.clone(),
                _ => Vec::new(),
            },
            "rollout" => match args.first() {
                None => Self::owned(KUBECTL_ROLLOUT),
                Some(_) if args.len() == 1 => ["deployment", "statefulset", "daemonset"]
                    .iter()
                    .flat_map(|kind| self.names_of(kind).map(move |name| format!("{kind}/{name}")))
                    .collect(),
                _ => Vec::new(),
            },
            "top" if args.is_empty() => Self::owned(&["pods", "nodes"]),
            "top" => args.first().and_then(|a| Self::canonical_kind(a)).map(|k| self.names_of(k).map(String::from).collect()).unwrap_or_default(),
            "logs" | "exec" | "attach" | "port-forward" | "cp" | "debug" if args.is_empty() => {
                let mut names: Vec<String> = self.names_of("pod").map(String::from).collect();
                names.extend(["deploy/", "svc/", "sts/"].map(String::from));
                names
            }
            "cordon" | "uncordon" | "drain" if args.is_empty() => self.names_of("node").map(String::from).collect(),
            "get" | "describe" | "delete" | "edit" | "label" | "annotate" | "scale" | "explain" | "set" => {
                match args.first().and_then(|a| Self::canonical_kind(a)) {
                    Some("namespace") => self.namespaces.clone(),
                    Some(kind) => self.names_of(kind).map(String::from).collect(),
                    None if args.is_empty() => {
                        let mut kinds: Vec<String> = KINDS.iter().flat_map(|(_, aliases)| aliases.iter().map(|a| a.to_string())).collect();
                        kinds.push("all".into());
                        kinds
                    }
                    None => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    fn complete_helm(&self, words: &[&str], current: &str) -> Vec<String> {
        if let Some(values) = self.flag_values(words) {
            return values;
        }
        if current.starts_with('-') {
            return Self::owned(HELM_FLAGS);
        }
        let positional = Self::positional(words);
        let Some(&verb) = positional.first() else {
            return Self::owned(HELM_VERBS);
        };
        let args = &positional[1..];
        match verb {
            "get" if args.is_empty() => Self::owned(HELM_GET),
            "get" if args.len() == 1 => self.releases.clone(),
            "repo" if args.is_empty() => Self::owned(HELM_REPO),
            "status" | "history" | "rollback" | "uninstall" | "upgrade" | "diff" if args.is_empty() => self.releases.clone(),
            _ => Vec::new(),
        }
    }

    /// Values for the flag right before the cursor (`-n <TAB>`, `--context <TAB>`).
    fn flag_values(&self, words: &[&str]) -> Option<Vec<String>> {
        match words.last() {
            Some(&("-n" | "--namespace")) => Some(self.namespaces.clone()),
            Some(&"--context") => Some(self.contexts.clone()),
            Some(&"-o") => Some(Self::owned(&["wide", "yaml", "json", "name", "jsonpath="])),
            _ => None,
        }
    }

    /// Non-flag words, skipping the values of flags like `-n ns`.
    fn positional<'a>(words: &[&'a str]) -> Vec<&'a str> {
        let mut positional = Vec::new();
        let mut skip_next = false;
        for word in words {
            if skip_next {
                skip_next = false;
            } else if word.starts_with('-') {
                skip_next = VALUE_FLAGS.contains(word);
            } else {
                positional.push(*word);
            }
        }
        positional
    }

    fn canonical_kind(alias: &str) -> Option<&'static str> {
        let alias = alias.split('.').next().unwrap_or(alias).to_lowercase();
        KINDS
            .iter()
            .find(|(kind, aliases)| *kind == alias || aliases.contains(&alias.as_str()))
            .map(|(kind, _)| *kind)
    }

    fn names_of<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.resources.iter().filter(move |(k, _)| k == kind).map(|(_, name)| name.as_str())
    }

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// Prefix matches first, then substring matches; deduplicated and capped.
    fn rank(candidates: Vec<String>, current: &str) -> Vec<String> {
        let needle = current.to_lowercase();
        let (mut prefix, contains): (Vec<String>, Vec<String>) = candidates
            .into_iter()
            .filter(|c| c.to_lowercase().contains(&needle) && c != current)
            .partition(|c| c.to_lowercase().starts_with(&needle));
        prefix.extend(contains);
        let mut seen = std::collections::HashSet::new();
        prefix.retain(|c| seen.insert(c.clone()));
        prefix.truncate(MAX_SUGGESTIONS);
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completer() -> Completer {
        let mut c = Completer::default();
        c.set_resources("pod/xxx-service-7d9f-abc\nservice/xxx-service\ndeployment.apps/xxx-service\ndeployment.apps/billing\n");
        c.namespaces = vec!["default".into(), "shop".into()];
        c
    }

    #[test]
    fn completes_alias_with_deployment_names() {
        let mut input = "ked xxx-ser".to_string();
        completer().tab(&mut input, false);
        assert_eq!(input, "ked xxx-service ");
    }

    #[test]
    fn completes_kind_slash_name_and_namespaces() {
        let c = completer();
        assert_eq!(c.complete("kubectl logs svc/xx").1, ["svc/xxx-service"]);
        assert_eq!(c.complete("kubectl get pods -n sh").1, ["shop"]);
        assert!(c.complete("kubectl get ").1.contains(&"svc".to_string()));
    }

    #[test]
    fn tab_cycles_through_ambiguous_candidates() {
        let mut c = completer();
        let mut input = "kubectl describe deploy ".to_string();
        c.tab(&mut input, false);
        let first = input.clone();
        c.tab(&mut input, false);
        assert_ne!(first, input);
        assert!(input.starts_with("kubectl describe deploy "));
    }

    #[test]
    fn expands_aliases() {
        assert_eq!(helpers::expand_alias("ked web"), "kubectl edit deployment web");
        assert_eq!(helpers::expand_alias("kubectl get pods"), "kubectl get pods");
        assert_eq!(helpers::expand_alias("watch kgp"), "watch kubectl get pods");
        assert_eq!(helpers::expand_alias("watch -n 5 kgp -A"), "watch -n 5 kubectl get pods -A");
    }

    #[test]
    fn completes_pod_names_after_oh_my_zsh_aliases() {
        let c = completer();
        assert_eq!(c.complete("klf xxx").1, ["xxx-service-7d9f-abc"]);
        assert_eq!(c.complete("kl xxx").1, ["xxx-service-7d9f-abc"]);
        assert_eq!(c.complete("watch klf xxx").1, ["xxx-service-7d9f-abc"]);
        assert!(c.complete("kl").1.contains(&"klf".to_string()));
    }

    fn has(candidates: &[String], word: &str) -> bool {
        candidates.iter().any(|c| c == word)
    }

    #[test]
    fn completes_every_kubectl_position() {
        let mut c = completer();
        c.contexts = vec!["prod".into(), "staging".into()];
        c.set_resources("pod/web-1\ndeployment.apps/web\nnode/worker-1\n");

        assert!(has(&c.complete("").1, "klf"));
        assert!(has(&c.complete("kubectl ").1, "rollout"));
        assert!(has(&c.complete("kubectl get pods --").1, "--show-labels"));
        assert_eq!(c.complete("kubectl get pods --context ").1, ["prod", "staging"]);
        assert!(has(&c.complete("kubectl get pods -o ").1, "yaml"));
        assert!(c.complete("kubectl logs nope/x").1.is_empty());
        assert!(has(&c.complete("kubectl config ").1, "use-context"));
        assert_eq!(c.complete("kubectl config use-context ").1, ["prod", "staging"]);
        assert!(c.complete("kubectl config view ").1.is_empty());
        assert!(has(&c.complete("kubectl rollout ").1, "restart"));
        assert_eq!(c.complete("kubectl rollout restart ").1, ["deployment/web"]);
        assert!(c.complete("kubectl rollout restart deployment/web ").1.is_empty());
        assert_eq!(c.complete("kubectl top ").1, ["pods", "nodes"]);
        assert_eq!(c.complete("kubectl top nodes ").1, ["worker-1"]);
        assert!(c.complete("kubectl top nonsense ").1.is_empty());
        assert!(has(&c.complete("kubectl logs ").1, "svc/"));
        assert_eq!(c.complete("kubectl drain ").1, ["worker-1"]);
        assert_eq!(c.complete("kubectl get ns ").1, ["default", "shop"]);
        assert_eq!(c.complete("kubectl get deployment.apps ").1, ["web"]);
        assert!(has(&c.complete("kubectl get ").1, "all"));
        assert!(c.complete("kubectl get nonsense ").1.is_empty());
        assert!(c.complete("kubectl apply ").1.is_empty());
        assert!(c.complete("ls ").1.is_empty());
        assert!(has(&c.complete("watch -n 5 kubectl ").1, "get"));
    }

    #[test]
    fn completes_helm_and_flux() {
        let mut c = completer();
        c.releases = vec!["web".into()];
        c.flux_names = vec!["apps".into()];
        assert_eq!(c.complete("helm status -n ").1, ["default", "shop"]);
        assert!(has(&c.complete("helm upgrade --").1, "--atomic"));
        assert!(has(&c.complete("helm ").1, "rollback"));
        assert!(has(&c.complete("helm get ").1, "manifest"));
        assert_eq!(c.complete("helm get values ").1, ["web"]);
        assert!(has(&c.complete("helm repo ").1, "update"));
        for verb in ["status", "history", "rollback", "uninstall", "upgrade", "diff"] {
            assert_eq!(c.complete(&format!("helm {verb} ")).1, ["web"]);
        }
        assert!(c.complete("helm lint ./chart ").1.is_empty());
        assert!(c.complete("helm get values web ").1.is_empty());
        assert!(c.complete("helm repo add ").1.is_empty());
        assert!(c.complete("helm status web ").1.is_empty());
        assert_eq!(c.complete("helm status -n shop ").1, ["web"]);
        assert!(has(&c.complete("flux ").1, "reconcile"));
        assert!(has(&c.complete("flux reconcile ").1, "kustomization"));
        assert!(has(&c.complete("flux reconcile source ").1, "git"));
        assert_eq!(c.complete("flux suspend kustomization ").1, ["apps"]);
    }

    #[test]
    fn tab_inserts_cycles_and_shows_suggestions() {
        let mut c = completer();
        let mut input = "kubectl zzz".to_string();
        c.tab(&mut input, false);
        assert_eq!(input, "kubectl zzz");

        let mut input = "kubectl describe deploy ".to_string();
        assert_eq!(c.suggestions(&input), (vec!["xxx-service".to_string(), "billing".to_string()], None));
        c.tab(&mut input, true);
        assert_eq!(input, "kubectl describe deploy billing");
        assert_eq!(c.suggestions(&input).1, Some(1));
        c.tab(&mut input, true);
        assert_eq!(input, "kubectl describe deploy xxx-service");
        c.reset();
        assert_eq!(c.suggestions("kubectl describe deploy xxx-service").1, None);
    }
}
