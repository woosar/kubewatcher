# kubewatcher

**A terminal UI for kubectl, helm and flux that teaches you the commands while you use it.**

Kubernetes is hard when you're the only one doing it. You need dozens of commands, half of them
with flags you can never remember, and GUIs hide what they actually do. kubewatcher sits in the
middle: you click around a friendly TUI, and **every action shows you the real command it runs**,
asks before changing anything, and writes it to a journal you can look back at.

After a few weeks you'll notice you can type most of it yourself.

```sh
cargo install kubewatcher
kubewatcher
```

## What you get

- **The one tool you need.** On first start kubewatcher checks for kubectl, kubectx/kubens, fzf,
  helm, flux, the kubeconfig layout and the shell wiring. If anything is missing, it says so and only
  the **Tooling tab (5)** works until it's installed. That tab shows installed and newest versions and
  installs or updates each tool into `~/.local/bin` with the download, sha256 check and `install`
  commands shown first (`Enter`/`u` one, `U` everything, `r` re-check). The shell wiring adds
  `kubeconfig_rebuild`, `~/.local/bin` on `PATH`, `kx`/`kns` and `kgp` & co. to your `.zshrc`/`.bashrc`
  (or accepts devsetup's block; with oh-my-zsh it enables the `kubectl` plugin).
- **One file per context.** On the Contexts tab, `a` asks for name, server (IP or URL), namespace and
  token and writes them with `kubectl config set-cluster/set-credentials/set-context` into
  `~/.kube/config.d/<name>.yaml`. The FILE column shows where each context lives, `x` removes a
  context that has a file of its own, and `S` splits files that hold several contexts (a backup is
  written first; `~/.kube/config` keeps only `current-context`, since kubectx/kubens write there).
- **Contexts, namespaces, resources, Helm releases and Flux objects** in tabs, with filtering (`/`).
- **Help with Flux, where GitOps gets confusing** (details [below](#flux)).
- **Nothing hidden.** Pressing `r` on a Flux kustomization shows and runs
  `flux reconcile kustomization web -n apps --with-source`. Turning Flux off is
  `flux suspend kustomization --all -n apps`. You see it before it runs.
- **Every change asks first.** The confirm dialog shows the exact command: `y` run it,
  `e` edit it in the prompt first, `n` cancel.
- **A journal of everything.** Each command, with time, duration and result, saved to
  `~/.local/state/kubewatcher/journal/YYYY-MM-DD.log`. Use `c` for "changes only" when you need to
  answer "what did I do to the cluster yesterday?"
- **A command prompt (`:`) with Tab completion** for verbs, kinds and live resource names, plus the
  oh-my-zsh kubectl aliases (`kgp`, `klf`, `ked`, …). It shows the expansion as you type
  (`klf web` → `kubectl logs -f web`).
- **A searchable cheatsheet (tab 7)** with about 120 commands for kubectl, helm and flux, each
  explained in one line. Enter loads one into the prompt with your current namespace filled in.
- **Live logs, `watch`, port-forwards and rollout status** in a scrollable viewer.
- **Themes** (`T`): default, Violet and Osaka Jade. Add your own in `~/.config/kubewatcher/themes/`.

## Flux

Flux keeps changing things after you've changed them, and its problems often show up far from their
cause. The Flux tab (`4`) is built for that. Press `?` there for all its keys.

- **See what's wrong first.** It opens on an overview of every Flux object, most urgent first (`a` shows
  only problems). All Flux's kinds are listed too, and one whose controller your cluster doesn't run says
  so instead of showing an error.
- **A banner on every tab** names whatever is suspended or failing, so a paused object can't be forgotten.
- **Who manages this?** `t` on any resource in the Resources tab runs `flux trace`: why does my change
  keep coming back, and from where?
- **Failures in plain language.** Common errors are explained under the list with the next command to
  try; `p` loads it into the prompt, so you decide whether to run it.
- **Experiment safely.** `X` suspends an object so you can change things live. `X` again lists what you
  changed and what resuming will do to it, which differs per kind: a Kustomization overwrites your edits,
  while a HelmRelease without `driftDetection` silently keeps them. It warns before you quit and refuses to
  switch cluster while an experiment runs.
- **Follow the chain.** `f` shows what an object depends on; Enter jumps there.
- **Stuck HelmReleases.** `v` final values (never written to the journal), `h` history, `i` status, `Z`
  reset the failure count, `F` force an upgrade.
- **Helm and Flux together.** Flux can keep a release's Helm records in another namespace, where
  plain `helm status` won't find it. The Helm tab looks in the right place, and warns before a rollback or
  uninstall that Flux may undo.
- **Create objects as YAML.** `c` builds a `flux create … --export` command. It only prints YAML or saves
  it to a new file: nothing reaches the cluster until you commit it.
- **Preview a push.** `D` runs `flux diff` of a Kustomization against a local folder.
- **Controller health.** `K` shows the controllers' pods, `flux stats` and their recent errors. `C` runs
  `flux check`.

## Requirements

kubewatcher doesn't talk to the cluster itself. It runs the same tools you would, and installs
them for you on the Tooling tab: kubectl, kubectx/kubens, fzf, helm and flux.

To install them it needs `curl`, `sha256sum`, `tar` and a POSIX `sh`. Built and tested for
**Linux** (x86_64 and arm64). Windows is not supported.

Whatever `kubectl get pods` can see in your shell, kubewatcher can see.

## Keys

Press `?` in the app for the full list. To get started:

| Key | What it does |
|---|---|
| `1`–`7` | Contexts · Resources · Helm · Flux · Tooling · Journal · Cheatsheet |
| `Space` | open any view (`Space p` pods, `Space d` deployments, `Space F` flux, …) |
| `n` | pick a namespace |
| `/` | filter the list |
| `:` | type any command (Tab completes) |
| `d` / `y` / `l` | describe / yaml / logs of the selected item |
| `J` | show/hide the "commands run" strip |
| `q` | quit |

## Files

| Path | What |
|---|---|
| `~/.local/state/kubewatcher/journal/` | one log per day of every command run |
| `~/.config/kubewatcher/themes/*.toml` | color themes (edit or add your own) |
| `~/.config/kubewatcher/config.toml` | your chosen theme |
| `~/.kube/config.d/<context>.yaml` | one kubeconfig per context; kubewatcher sets `KUBECONFIG` to `~/.kube/config` plus these for every command it runs |
| `~/.local/bin/` | kubectl, kubectx, kubens, fzf, helm and flux installed from the Tooling tab |
| `~/.zshrc` / `~/.bashrc` | the `# >>> kubewatcher >>>` block (unless devsetup's block is there) |

`$XDG_STATE_HOME` and `$XDG_CONFIG_HOME` are respected.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
