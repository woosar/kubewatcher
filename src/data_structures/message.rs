/// Which part of the UI a finished background command belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Contexts,
    Namespaces,
    /// Resource listing; the id lets stale results (old kind/namespace) be dropped.
    Resources(u64),
    Helm(u64),
    Flux(u64),
    /// Output for the viewer opened with this generation id.
    Viewer(u64),
    /// `kubectl get ... -o name` for prompt completion.
    Completions,
    /// A one-shot mutating action; the result goes to the status bar.
    Action,
    /// A Tooling row's installed version, or with `latest` its newest release.
    Tooling { row: usize, latest: bool },
    /// `e` on a workload: its pod selector, then its pods (lookup id).
    PodSelector(u64),
    PodList(u64),
}

/// Messages sent from background command threads to the UI loop.
#[derive(Debug, PartialEq)]
pub enum Msg {
    Output {
        target: Target,
        journal: u64,
        result: Result<String, String>,
    },
    Line {
        generation: u64,
        line: String,
    },
    StreamEnd {
        generation: u64,
    },
}
