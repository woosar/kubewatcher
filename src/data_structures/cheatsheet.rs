use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::{Cell, Row, Table, TableState};

use crate::data_structures::Theme;
use crate::helpers;

struct CheatEntry {
    section: &'static str,
    command: &'static str,
    description: &'static str,
}

const fn entry(section: &'static str, command: &'static str, description: &'static str) -> CheatEntry {
    CheatEntry { section, command, description }
}

/// `<ns>` is replaced with the active namespace when a command is loaded into the prompt.
const ENTRIES: &[CheatEntry] = &[
    // contexts & config
    entry("context", "kubectl config get-contexts", "List all contexts in your kubeconfig"),
    entry("context", "kubectl config current-context", "Show the active context"),
    entry("context", "kubectl config use-context <context>", "Switch context (what kubectx does)"),
    entry("context", "kubectl config set-context --current --namespace=<ns>", "Set default namespace of the current context (kubens)"),
    entry("context", "kubectl config rename-context <old> <new>", "Rename a context"),
    entry("context", "kubectl config delete-context <context>", "Remove a context from kubeconfig"),
    entry("context", "kubectl config view --minify", "Show config of the current context only"),
    entry("context", "KUBECONFIG=~/.kube/config:~/new.yaml kubectl config view --flatten > /tmp/merged.yaml", "Merge two kubeconfig files"),
    entry("context", "kubectl cluster-info", "API server and core service endpoints"),
    entry("context", "kubectl auth can-i --list -n <ns>", "What am I allowed to do here?"),
    // inspect
    entry("inspect", "kubectl get all -n <ns>", "Common workload resources in a namespace"),
    entry("inspect", "kubectl get pods -A -o wide", "All pods in all namespaces with node + IP"),
    entry("inspect", "kubectl get pods -n <ns> --field-selector=status.phase!=Running", "Pods that are not running"),
    entry("inspect", "kubectl get pods -n <ns> -l app=<label>", "Filter by label selector"),
    entry("inspect", "kubectl get pods -n <ns> --show-labels", "Show labels as a column"),
    entry("inspect", "kubectl get pods -n <ns> --sort-by=.status.containerStatuses[0].restartCount", "Sort pods by restart count"),
    entry("inspect", "kubectl describe pod <pod> -n <ns>", "Full details incl. events (why is it pending/crashing?)"),
    entry("inspect", "kubectl get deploy <deploy> -n <ns> -o yaml", "Raw manifest as stored in the cluster"),
    entry("inspect", "kubectl get pod <pod> -n <ns> -o jsonpath='{.spec.containers[*].image}'", "Extract one field with jsonpath"),
    entry("inspect", "kubectl explain deployment.spec.strategy", "Built-in docs for any resource field"),
    entry("inspect", "kubectl api-resources", "All resource kinds + short names"),
    entry("inspect", "kubectl get events -n <ns> --sort-by=.lastTimestamp", "Recent events, newest last"),
    entry("inspect", "kubectl get events -A --field-selector type=Warning", "Only warnings, cluster-wide"),
    // logs
    entry("logs", "kubectl logs -f <pod> -n <ns>", "Follow logs of a pod"),
    entry("logs", "kubectl logs -f deploy/<deploy> -n <ns> --all-containers --prefix", "Follow logs of a deployment (one pod)"),
    entry("logs", "kubectl logs -f -l app=<label> -n <ns> --max-log-requests=10 --prefix", "Follow logs of all pods matching a label"),
    entry("logs", "kubectl logs <pod> -n <ns> --previous", "Logs of the crashed previous container"),
    entry("logs", "kubectl logs <pod> -n <ns> -c <container> --since=15m", "One container, last 15 minutes"),
    entry("logs", "kubectl logs <pod> -n <ns> --tail=100 --timestamps", "Last 100 lines with timestamps"),
    // debug
    entry("debug", "kubectl exec -it <pod> -n <ns> -- sh", "Shell into a pod (prefix with ! in the prompt)"),
    entry("debug", "kubectl exec <pod> -n <ns> -- env", "Run a single command in a pod"),
    entry("debug", "kubectl debug -it <pod> -n <ns> --image=busybox --target=<container>", "Ephemeral debug container (distroless images)"),
    entry("debug", "kubectl run tmp-shell --rm -it -n <ns> --image=nicolaka/netshoot -- bash", "Throwaway pod with network tools"),
    entry("debug", "kubectl port-forward svc/<svc> 8080:80 -n <ns>", "Reach a service on localhost:8080"),
    entry("debug", "kubectl cp <ns>/<pod>:/path/file ./file", "Copy a file out of a pod"),
    entry("debug", "kubectl top pods -n <ns> --sort-by=memory", "CPU/memory usage (needs metrics-server)"),
    entry("debug", "kubectl top nodes", "Node resource usage"),
    entry("debug", "kubectl get endpoints <svc> -n <ns>", "Does the service actually select any pods?"),
    // deploy & rollout
    entry("rollout", "kubectl apply -f <file.yaml>", "Create/update resources from a file"),
    entry("rollout", "kubectl diff -f <file.yaml>", "Preview what apply would change"),
    entry("rollout", "kubectl rollout restart deploy/<deploy> -n <ns>", "Restart all pods of a deployment"),
    entry("rollout", "kubectl rollout status deploy/<deploy> -n <ns>", "Wait for a rollout to finish"),
    entry("rollout", "kubectl rollout history deploy/<deploy> -n <ns>", "Revision history"),
    entry("rollout", "kubectl rollout undo deploy/<deploy> -n <ns>", "Roll back to the previous revision"),
    entry("rollout", "kubectl set image deploy/<deploy> <container>=<image>:<tag> -n <ns>", "Change an image in place"),
    entry("rollout", "kubectl scale deploy/<deploy> --replicas=3 -n <ns>", "Scale a deployment"),
    entry("rollout", "kubectl autoscale deploy/<deploy> --min=2 --max=5 --cpu-percent=80 -n <ns>", "Create an HPA"),
    entry("rollout", "kubectl delete pod <pod> -n <ns>", "Delete a pod (its controller recreates it)"),
    entry("rollout", "kubectl delete pod <pod> -n <ns> --grace-period=0 --force", "Force-kill a stuck terminating pod"),
    // config & secrets
    entry("config", "kubectl create configmap <name> --from-file=<file> -n <ns>", "ConfigMap from a file"),
    entry("config", "kubectl create secret generic <name> --from-literal=KEY=value -n <ns>", "Secret from literals"),
    entry("config", "kubectl create secret docker-registry regcred --docker-server=<server> --docker-username=<user> --docker-password=<pw> -n <ns>", "Image pull secret"),
    entry("config", "kubectl get secret <name> -n <ns> -o jsonpath='{.data.KEY}' | base64 -d", "Decode one secret value"),
    entry("config", "kubectl get secret <name> -n <ns> -o go-template='{{range $k,$v := .data}}{{$k}}={{$v|base64decode}}{{\"\\n\"}}{{end}}'", "Decode all secret values"),
    entry("config", "kubectl create secret generic <name> --from-literal=K=v --dry-run=client -o yaml", "Generate YAML without creating anything"),
    // nodes
    entry("nodes", "kubectl get nodes -o wide", "Nodes with IPs, OS, runtime"),
    entry("nodes", "kubectl describe node <node>", "Capacity, allocations, conditions"),
    entry("nodes", "kubectl cordon <node>", "Stop scheduling onto a node"),
    entry("nodes", "kubectl drain <node> --ignore-daemonsets --delete-emptydir-data", "Evict pods for maintenance"),
    entry("nodes", "kubectl uncordon <node>", "Allow scheduling again"),
    entry("nodes", "kubectl get pods -A --field-selector spec.nodeName=<node>", "Pods running on a node"),
    // helm repos & charts
    entry("helm repo", "helm repo add <repo> <url>", "Add a chart repository"),
    entry("helm repo", "helm repo update", "Refresh chart indexes"),
    entry("helm repo", "helm repo list", "Configured repositories"),
    entry("helm repo", "helm search repo <keyword> --versions", "Find charts and their versions"),
    entry("helm repo", "helm show values <repo>/<chart>", "Default values of a chart"),
    entry("helm repo", "helm show values <repo>/<chart> > values.yaml", "Start your own values file"),
    entry("helm repo", "helm pull <repo>/<chart> --untar", "Download a chart locally"),
    // helm releases
    entry("helm", "helm list -A", "All releases in all namespaces"),
    entry("helm", "helm list -n <ns> --all", "Include failed/pending releases"),
    entry("helm", "helm upgrade --install <release> <repo>/<chart> -n <ns> --create-namespace -f values.yaml", "Install or upgrade (idempotent)"),
    entry("helm", "helm upgrade <release> <repo>/<chart> -n <ns> --reuse-values --set image.tag=<tag>", "Change one value, keep the rest"),
    entry("helm", "helm upgrade --install <release> <chart> -n <ns> --atomic --timeout 5m", "Auto-rollback if the upgrade fails"),
    entry("helm", "helm status <release> -n <ns>", "Release status + notes"),
    entry("helm", "helm history <release> -n <ns>", "Revision history"),
    entry("helm", "helm rollback <release> <revision> -n <ns>", "Roll back to a revision"),
    entry("helm", "helm uninstall <release> -n <ns>", "Remove a release"),
    entry("helm", "helm get values <release> -n <ns>", "User-supplied values"),
    entry("helm", "helm get values <release> -n <ns> --all", "All computed values"),
    entry("helm", "helm get manifest <release> -n <ns>", "Rendered Kubernetes manifests"),
    // helm debugging
    entry("helm debug", "helm template <release> <chart> -f values.yaml", "Render templates locally"),
    entry("helm debug", "helm upgrade --install <release> <chart> -n <ns> --dry-run=server --debug", "Simulate against the cluster"),
    entry("helm debug", "helm lint <chart-dir>", "Check a chart for problems"),
    entry("helm debug", "helm diff upgrade <release> <chart> -n <ns> -f values.yaml", "Show changes (needs helm-diff plugin)"),
    entry("helm debug", "kubectl get secrets -n <ns> -l owner=helm", "Helm's release records (stuck 'pending-upgrade'?)"),
    // flux (GitOps: git is the source of truth, Flux applies it to the cluster)
    entry("flux", "flux check", "Is Flux installed and healthy?"),
    entry("flux", "flux get all -A", "Every Flux object and whether it is Ready"),
    entry("flux", "flux get kustomizations -A", "What Flux applies from git, and the revision it applied"),
    entry("flux", "flux get helmreleases -A", "Helm charts managed by Flux"),
    entry("flux", "flux get sources git -A", "Git repositories Flux pulls from"),
    entry("flux", "flux reconcile kustomization flux-system -n flux-system --with-source", "Pull git now instead of waiting for the interval"),
    entry("flux", "flux reconcile helmrelease <release> -n <ns> --with-source", "Re-apply a HelmRelease now"),
    entry("flux", "flux suspend kustomization <name> -n <ns>", "Pause Flux for one kustomization (manual changes stick)"),
    entry("flux", "flux resume kustomization <name> -n <ns>", "Un-pause: Flux re-applies git state"),
    entry("flux", "flux suspend kustomization --all -n flux-system", "Pause all kustomizations in a namespace (Flux 'off')"),
    entry("flux", "flux suspend helmrelease <release> -n <ns>", "Pause Flux for one HelmRelease"),
    entry("flux", "flux tree kustomization flux-system -n flux-system", "Everything a kustomization manages"),
    entry("flux", "flux events -A", "Recent Flux events"),
    entry("flux", "flux logs -A --level=error", "Errors from all Flux controllers"),
    entry("flux", "flux diff kustomization <name> --path ./clusters/<cluster>", "What would change if you pushed this local path"),
    entry("flux", "flux create source helm <repo> --url=<url> --interval=1h --export", "Generate a HelmRepository YAML (commit it)"),
    entry("flux", "flux create helmrelease <release> --chart=<chart> --source=HelmRepository/<repo> --target-namespace=<ns> --export", "Generate a HelmRelease YAML (commit it)"),
    entry("flux", "kubectl get <kind> <name> -n <ns> -o jsonpath='{.metadata.labels}'", "Managed by Flux? Look for kustomize.toolkit.fluxcd.io/name"),
    // watch
    entry("watch", "watch kubectl get pods -n <ns>", "Re-run every 2s (built-in watch in kubewatcher)"),
    entry("watch", "watch -n 5 kubectl top pods -n <ns>", "Re-run every 5s"),
    entry("watch", "kubectl get pods -n <ns> -w", "Stream changes (live in the viewer)"),
    entry("watch", "kubectl get events -n <ns> -w", "Stream events as they happen"),
];

/// Searchable list of kubectl/helm commands; Enter loads one into the command prompt.
pub struct Cheatsheet {
    pub filter: String,
    state: TableState,
}

impl Cheatsheet {
    pub fn new() -> Self {
        Self {
            filter: String::new(),
            state: TableState::default().with_selected(Some(0)),
        }
    }

    fn visible(&self) -> Vec<&'static CheatEntry> {
        ENTRIES
            .iter()
            .filter(|e| helpers::matches_filter([e.section, e.command, e.description], &self.filter))
            .collect()
    }

    pub fn step(&mut self, delta: isize) {
        let len = self.visible().len();
        self.state.select(helpers::step_selection(self.state.selected(), len, delta));
    }

    pub fn clamp(&mut self) {
        let len = self.visible().len();
        self.state.select(helpers::clamp_selection(self.state.selected(), len));
    }

    pub fn selected_command(&self) -> Option<&'static str> {
        self.state
            .selected()
            .and_then(|i| self.visible().get(i).map(|e| e.command))
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let visible = self.visible();
        let rows = visible.iter().map(|e| {
            Row::new([
                Cell::from(e.section).style(theme.fg(theme.warning)),
                Cell::from(e.command).style(theme.fg(theme.success)),
                Cell::from(e.description),
            ])
        });
        let title = if self.filter.is_empty() {
            format!(" Cheatsheet ({}) ", visible.len())
        } else {
            format!(" Cheatsheet ({}) /{} ", visible.len(), self.filter)
        };
        let block = theme
            .block()
            .title(title)
            .title_bottom(Line::from(" Enter: load into prompt · <ns> = current namespace · <...> = fill in ").dim());
        let header = Row::new(["SECTION", "COMMAND", "WHAT IT DOES"])
            .style(theme.header());
        let table = Table::new(rows, [Constraint::Length(10), Constraint::Percentage(60), Constraint::Fill(1)])
            .header(header)
            .block(block)
            .column_spacing(2)
            .row_highlight_style(theme.selected())
            .highlight_symbol("▶ ");
        frame.render_stateful_widget(table, area, &mut self.state);
    }
}
