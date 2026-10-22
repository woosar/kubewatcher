use std::path::PathBuf;

use crate::data_structures::Machine;
use crate::helpers::{self, argv};

pub const DEVSETUP_OPEN: &str = "# >>> devsetup >>>";
pub const BLOCK_OPEN: &str = "# >>> kubewatcher >>>";
pub const BLOCK_CLOSE: &str = "# <<< kubewatcher <<<";

/// Same `kubeconfig_rebuild` devsetup writes: ~/.kube/config stays the write target and
/// every *.yaml in ~/.kube/config.d is merged on top, one file per context.
const REBUILD_ZSH: &str = r#"kubeconfig_rebuild() {
  local -a files
  files=("$HOME/.kube/config" "$HOME"/.kube/config.d/*.yaml(N))
  export KUBECONFIG="${(j.:.)files}"
}"#;
const REBUILD_BASH: &str = r#"kubeconfig_rebuild() {
  local f files="$HOME/.kube/config"
  for f in "$HOME"/.kube/config.d/*.yaml; do
    [ -e "$f" ] && files="$files:$f"
  done
  export KUBECONFIG="$files"
}"#;
/// Without oh-my-zsh's kubectl plugin, the few aliases used most.
const ALIASES: &str = "alias k=kubectl
alias kgp='kubectl get pods'
alias kgs='kubectl get svc'
alias kgd='kubectl get deployment'
alias kgn='kubectl get nodes'
alias kl='kubectl logs'
alias kd='kubectl describe'";

/// One row of the Tooling tab.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolKind {
    Kubectl,
    Kubectx,
    Fzf,
    Helm,
    Flux,
    /// `~/.kube/config.d` (0700) and `~/.kube/config` (0600).
    Kubeconfig,
    /// The rc block with `kubeconfig_rebuild`, `~/.local/bin` on PATH and the aliases.
    Shell,
}

/// A tool kubewatcher needs, what is installed and what the newest release is.
#[derive(Clone, Debug)]
pub struct Tool {
    pub kind: ToolKind,
    /// Where the program was found (binaries only).
    pub location: Option<PathBuf>,
    pub present: bool,
    pub installed: Option<String>,
    /// Release tag of the newest version, e.g. `v1.34.1`.
    pub latest: Option<String>,
    /// Why a version could not be read.
    pub problem: Option<String>,
}

impl Tool {
    pub const KINDS: [ToolKind; 7] =
        [ToolKind::Kubectl, ToolKind::Kubectx, ToolKind::Fzf, ToolKind::Helm, ToolKind::Flux, ToolKind::Kubeconfig, ToolKind::Shell];

    /// Every tool, with its presence checked (no network, no processes).
    pub fn all(machine: &Machine) -> Vec<Tool> {
        Self::KINDS
            .iter()
            .map(|&kind| {
                let mut tool = Tool { kind, location: None, present: false, installed: None, latest: None, problem: None };
                tool.check(machine);
                tool
            })
            .collect()
    }

    pub fn name(&self) -> &'static str {
        match self.kind {
            ToolKind::Kubectl => "kubectl",
            ToolKind::Kubectx => "kubectx + kubens",
            ToolKind::Fzf => "fzf",
            ToolKind::Helm => "helm",
            ToolKind::Flux => "flux",
            ToolKind::Kubeconfig => "kubeconfig layout",
            ToolKind::Shell => "shell wiring",
        }
    }

    fn programs(&self) -> &'static [&'static str] {
        match self.kind {
            ToolKind::Kubectl => &["kubectl"],
            ToolKind::Kubectx => &["kubectx", "kubens"],
            ToolKind::Fzf => &["fzf"],
            ToolKind::Helm => &["helm"],
            ToolKind::Flux => &["flux"],
            ToolKind::Kubeconfig | ToolKind::Shell => &[],
        }
    }

    fn is_binary(&self) -> bool {
        !self.programs().is_empty()
    }

    /// Re-reads whether the tool is there.
    pub fn check(&mut self, machine: &Machine) {
        match self.kind {
            ToolKind::Kubeconfig => {
                self.present = machine.config_d().is_dir() && machine.kube_config().is_file();
            }
            ToolKind::Shell => {
                let rc = std::fs::read_to_string(machine.rc_file()).unwrap_or_default();
                let block = rc.contains(DEVSETUP_OPEN) || rc.contains(BLOCK_OPEN);
                self.present = block && (!machine.has_oh_my_zsh() || helpers::omz_has_plugin(&rc, "kubectl"));
            }
            _ => {
                let found: Vec<Option<PathBuf>> =
                    self.programs().iter().map(|p| helpers::find_program(p, &machine.search_path)).collect();
                self.location = found.first().cloned().flatten();
                self.present = found.iter().all(Option::is_some);
                if !self.present {
                    self.installed = None;
                }
            }
        }
    }

    /// Installed outside `~/.local/bin`: shown, but left to whoever installed it.
    pub fn is_system(&self, machine: &Machine) -> bool {
        self.location.as_ref().is_some_and(|l| !l.starts_with(machine.local_bin()))
    }

    /// Asks the installed binary for its version.
    pub fn version_argv(&self) -> Option<Vec<String>> {
        let program = self.location.as_ref()?.display().to_string();
        let args: &[&str] = match self.kind {
            ToolKind::Kubectl | ToolKind::Flux => &["version", "--client"],
            ToolKind::Kubectx | ToolKind::Fzf => &["--version"],
            ToolKind::Helm => &["version", "--template", "{{.Version}}"],
            ToolKind::Kubeconfig | ToolKind::Shell => return None,
        };
        let mut argv = vec![program];
        argv.extend(args.iter().map(|a| a.to_string()));
        Some(argv)
    }

    /// Looks up the newest release: kubectl's stable.txt, or where GitHub's
    /// `releases/latest` redirects to (no API token, no rate limit).
    pub fn latest_argv(&self) -> Option<Vec<String>> {
        let repo = match self.kind {
            ToolKind::Kubectl => return Some(argv(&["curl", "-fsSL", "https://dl.k8s.io/release/stable.txt"])),
            ToolKind::Kubectx => "ahmetb/kubectx",
            ToolKind::Fzf => "junegunn/fzf",
            ToolKind::Helm => "helm/helm",
            ToolKind::Flux => "fluxcd/flux2",
            ToolKind::Kubeconfig | ToolKind::Shell => return None,
        };
        let url = format!("https://github.com/{repo}/releases/latest");
        Some(argv(&["curl", "-fsSLI", "-o", "/dev/null", "-w", "%{url_effective}", &url]))
    }

    /// The tag from `latest_argv`'s output: `v1.34.1` or `…/releases/tag/v0.9.5`.
    pub fn parse_latest(text: &str) -> Option<String> {
        let tag = text.trim().rsplit('/').next()?;
        (tag.trim_start_matches('v').starts_with(|c: char| c.is_ascii_digit())).then(|| tag.to_string())
    }

    pub fn state(&self, machine: &Machine) -> &'static str {
        match (self.present, self.is_binary()) {
            (false, _) => "missing",
            (true, false) => "ok",
            _ if self.is_system(machine) => "system package",
            _ => match (&self.installed, &self.latest) {
                (Some(installed), Some(latest)) if helpers::same_version(installed, latest) => "ok",
                (Some(_), Some(_)) => "update available",
                _ => "checking…",
            },
        }
    }

    pub fn needs_install(&self, machine: &Machine) -> bool {
        matches!(self.state(machine), "missing" | "update available")
    }

    /// TOOL, INSTALLED, LATEST, LOCATION, STATE.
    pub fn row(&self, machine: &Machine) -> Vec<String> {
        let dash = || "-".to_string();
        let (installed, latest, location) = match self.kind {
            ToolKind::Kubeconfig => (dash(), dash(), machine.display(&machine.config_d())),
            ToolKind::Shell => (dash(), dash(), machine.display(&machine.rc_file())),
            _ => (
                self.installed.clone().unwrap_or_else(dash),
                self.latest.clone().or_else(|| self.problem.as_ref().map(|_| "?".into())).unwrap_or_else(dash),
                self.location.as_deref().map(|l| machine.display(l)).unwrap_or_else(dash),
            ),
        };
        vec![self.name().to_string(), installed, latest, location, self.state(machine).to_string()]
    }

    /// The download, checksum and install steps as one shell command line.
    /// Shell wiring is not a command: kubewatcher edits the rc file itself.
    pub fn install_command(&self, machine: &Machine) -> Result<String, String> {
        let dest = helpers::shell_join(&[machine.local_bin().display().to_string()]);
        if self.kind == ToolKind::Kubeconfig {
            let config_d = helpers::shell_join(&[machine.config_d().display().to_string()]);
            let config = helpers::shell_join(&[machine.kube_config().display().to_string()]);
            return Ok(format!(
                "mkdir -p {config_d} && chmod 700 {config_d} && {{ [ -f {config} ] || printf '%s\\n' 'apiVersion: v1' 'kind: Config' \
                 'clusters: []' 'contexts: []' 'users: []' > {config}; }} && chmod 600 {config}"
            ));
        }
        if self.kind == ToolKind::Shell {
            return Err("shell wiring is written by kubewatcher, not by a command".into());
        }
        let tag = self.latest.as_deref().ok_or("the latest version is not known yet; press r to check")?;
        let version = tag.trim_start_matches('v');
        let (go_arch, kubectx_arch) = match std::env::consts::ARCH {
            "x86_64" => ("amd64", "x86_64"),
            "aarch64" => ("arm64", "arm64"),
            other => return Err(format!("no {} release for this CPU ({other})", self.name())),
        };
        // (asset URL, checksum URL, file inside the archive or None for a plain binary)
        let downloads: Vec<(String, String, Option<String>)> = match self.kind {
            ToolKind::Kubectl => {
                let url = format!("https://dl.k8s.io/release/{tag}/bin/linux/{go_arch}/kubectl");
                vec![(url.clone(), format!("{url}.sha256"), None)]
            }
            ToolKind::Kubectx => ["kubectx", "kubens"]
                .iter()
                .map(|program| {
                    let base = format!("https://github.com/ahmetb/kubectx/releases/download/{tag}");
                    let asset = format!("{program}_{tag}_linux_{kubectx_arch}.tar.gz");
                    (format!("{base}/{asset}"), format!("{base}/checksums.txt"), Some(program.to_string()))
                })
                .collect(),
            ToolKind::Fzf => {
                let base = format!("https://github.com/junegunn/fzf/releases/download/{tag}");
                let asset = format!("fzf-{version}-linux_{go_arch}.tar.gz");
                vec![(format!("{base}/{asset}"), format!("{base}/fzf_{version}_checksums.txt"), Some("fzf".into()))]
            }
            ToolKind::Helm => {
                let url = format!("https://get.helm.sh/helm-{tag}-linux-{go_arch}.tar.gz");
                vec![(url.clone(), format!("{url}.sha256sum"), Some(format!("linux-{go_arch}/helm")))]
            }
            ToolKind::Flux => {
                let base = format!("https://github.com/fluxcd/flux2/releases/download/{tag}");
                let asset = format!("flux_{version}_linux_{go_arch}.tar.gz");
                vec![(format!("{base}/{asset}"), format!("{base}/flux_{version}_checksums.txt"), Some("flux".into()))]
            }
            ToolKind::Kubeconfig | ToolKind::Shell => unreachable!("handled above"),
        };
        let mut steps = vec![format!("mkdir -p {dest}"), "d=$(mktemp -d)".into(), "cd \"$d\"".into()];
        for (url, sums, entry) in downloads {
            let asset = url.rsplit('/').next().unwrap_or_default().to_string();
            steps.push(format!("curl -fsSLO {url}"));
            // Checksum files are either a bare hash or "hash  file" lines.
            steps.push(format!(
                "echo \"$(curl -fsSL {sums} | awk -v a={asset} 'NF==1 || $2==a || $2==\"*\"a {{print $1; exit}}')  {asset}\" | sha256sum -c -"
            ));
            let binary = match entry {
                Some(entry) => {
                    steps.push(format!("tar -xzf {asset} {entry}"));
                    entry
                }
                None => asset,
            };
            steps.push(format!("install -m 755 {binary} {dest}/"));
        }
        steps.push("cd / && rm -rf \"$d\"".into());
        Ok(steps.join(" && "))
    }

    /// The rc block kubewatcher writes when devsetup's is not there.
    pub fn shell_block(machine: &Machine) -> String {
        let rebuild = if machine.is_zsh() { REBUILD_ZSH } else { REBUILD_BASH };
        let aliases = if machine.has_oh_my_zsh() { "" } else { ALIASES };
        format!(
            "{BLOCK_OPEN}\n# Written by kubewatcher (Tooling tab); put your own lines outside this block.\n\
             case \":$PATH:\" in *\":$HOME/.local/bin:\"*) ;; *) PATH=\"$HOME/.local/bin:$PATH\" ;; esac\nexport PATH\n\
             # Split kubeconfig: ~/.kube/config is the write target, every *.yaml in ~/.kube/config.d is merged on top.\n\
             {rebuild}\n[ -d \"$HOME/.kube/config.d\" ] && kubeconfig_rebuild\nalias kx=kubectx\nalias kns=kubens\n{aliases}\n{BLOCK_CLOSE}\n"
        )
    }

    /// The rc file with the wiring in place (devsetup's block counts), or None if nothing changes.
    pub fn wired_rc(machine: &Machine, rc: &str) -> Option<String> {
        let mut text = rc.to_string();
        if !text.contains(DEVSETUP_OPEN) {
            text = helpers::with_block(&text, BLOCK_OPEN, BLOCK_CLOSE, &Self::shell_block(machine));
        }
        if machine.has_oh_my_zsh()
            && let Some(with_plugin) = helpers::add_omz_plugin(&text, "kubectl")
        {
            text = with_plugin;
        }
        (text != rc).then_some(text)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;

    use super::*;

    fn machine(name: &str, shell: &str) -> (Machine, PathBuf) {
        let dir = helpers::test_dir(name);
        let bin = dir.join("bin");
        fs::create_dir_all(dir.join("home")).unwrap();
        fs::create_dir_all(&bin).unwrap();
        (Machine::new(dir.join("home"), bin.clone().into_os_string(), shell), dir)
    }

    fn fake_program(dir: &std::path::Path, name: &str) {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn presence_follows_the_path_and_the_files() {
        let (machine, dir) = machine("tool-presence", "bash");
        let tools = Tool::all(&machine);
        assert!(tools.iter().all(|t| !t.present));
        assert!(tools.iter().all(|t| t.state(&machine) == "missing"));

        fake_program(&dir.join("bin"), "kubectl");
        fake_program(&machine.local_bin(), "kubectx");
        fake_program(&machine.local_bin(), "helm");
        let tools = Tool::all(&machine);
        let by_kind = |kind| tools.iter().find(|t| t.kind == kind).unwrap();
        assert!(by_kind(ToolKind::Kubectl).present && by_kind(ToolKind::Kubectl).is_system(&machine));
        assert_eq!(by_kind(ToolKind::Kubectl).state(&machine), "system package");
        assert!(!by_kind(ToolKind::Kubectx).present, "kubens is still missing");
        let mut helm = by_kind(ToolKind::Helm).clone();
        assert_eq!(helm.state(&machine), "checking…");
        helm.installed = Some("3.18.0".into());
        helm.latest = Some("v3.19.0".into());
        assert!(helm.needs_install(&machine));
        helm.installed = Some("v3.19.0".into());
        assert_eq!(helm.state(&machine), "ok");
        assert_eq!(helm.row(&machine)[3], "~/.local/bin/helm");

        fs::create_dir_all(machine.config_d()).unwrap();
        fs::write(machine.kube_config(), "").unwrap();
        fs::write(machine.rc_file(), format!("x\n{DEVSETUP_OPEN}\n")).unwrap();
        let tools = Tool::all(&machine);
        assert!(tools[5].present && tools[6].present);
        assert_eq!(tools[6].row(&machine), ["shell wiring", "-", "-", "~/.bashrc", "ok"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn version_and_latest_commands() {
        let mut tool = Tool { kind: ToolKind::Helm, location: Some("/b/helm".into()), present: true, installed: None, latest: None, problem: None };
        assert_eq!(tool.version_argv().unwrap(), ["/b/helm", "version", "--template", "{{.Version}}"]);
        assert_eq!(tool.latest_argv().unwrap().last().unwrap(), "https://github.com/helm/helm/releases/latest");
        tool.kind = ToolKind::Kubectl;
        assert_eq!(tool.latest_argv().unwrap(), ["curl", "-fsSL", "https://dl.k8s.io/release/stable.txt"]);
        tool.kind = ToolKind::Shell;
        assert!(tool.version_argv().is_none() && tool.latest_argv().is_none());
        assert_eq!(Tool::parse_latest("v1.34.1\n"), Some("v1.34.1".into()));
        assert_eq!(Tool::parse_latest("https://github.com/ahmetb/kubectx/releases/tag/v0.9.5"), Some("v0.9.5".into()));
        assert_eq!(Tool::parse_latest("https://github.com/x/releases"), None);
    }

    #[test]
    fn install_commands_verify_and_install_into_local_bin() {
        let machine = Machine::new(PathBuf::from("/h"), OsString::new(), "bash");
        let tool = |kind, latest: Option<&str>| Tool { kind, location: None, present: false, installed: None, latest: latest.map(String::from), problem: None };
        assert!(tool(ToolKind::Helm, None).install_command(&machine).unwrap_err().contains("press r"));
        assert!(tool(ToolKind::Shell, None).install_command(&machine).is_err());
        let layout = tool(ToolKind::Kubeconfig, None).install_command(&machine).unwrap();
        assert!(layout.starts_with("mkdir -p /h/.kube/config.d && chmod 700 /h/.kube/config.d && { [ -f /h/.kube/config ] ||"));
        assert!(layout.ends_with("chmod 600 /h/.kube/config"));
        if std::env::consts::ARCH != "x86_64" {
            return;
        }
        let kubectl = tool(ToolKind::Kubectl, Some("v1.34.1")).install_command(&machine).unwrap();
        assert!(kubectl.contains("curl -fsSLO https://dl.k8s.io/release/v1.34.1/bin/linux/amd64/kubectl && "));
        assert!(kubectl.contains("install -m 755 kubectl /h/.local/bin/"));
        let kubectx = tool(ToolKind::Kubectx, Some("v0.9.5")).install_command(&machine).unwrap();
        assert!(kubectx.contains("kubens_v0.9.5_linux_x86_64.tar.gz") && kubectx.contains("tar -xzf kubectx_v0.9.5_linux_x86_64.tar.gz kubectx"));
        let helm = tool(ToolKind::Helm, Some("v3.19.0")).install_command(&machine).unwrap();
        assert!(helm.contains("tar -xzf helm-v3.19.0-linux-amd64.tar.gz linux-amd64/helm && install -m 755 linux-amd64/helm /h/.local/bin/"));
        let flux = tool(ToolKind::Flux, Some("v2.7.0")).install_command(&machine).unwrap();
        assert!(flux.contains("flux_2.7.0_checksums.txt") && flux.contains("flux_2.7.0_linux_amd64.tar.gz"));
        let fzf = tool(ToolKind::Fzf, Some("v0.65.2")).install_command(&machine).unwrap();
        assert!(fzf.contains("fzf-0.65.2-linux_amd64.tar.gz") && fzf.ends_with("cd / && rm -rf \"$d\""));
    }

    #[test]
    fn shell_wiring_respects_devsetup_and_oh_my_zsh() {
        let (bash, dir) = machine("tool-shell", "bash");
        let wired = Tool::wired_rc(&bash, "export X=1\n").unwrap();
        assert!(wired.starts_with("export X=1\n") && wired.contains(BLOCK_OPEN) && wired.contains("alias kgp='kubectl get pods'"));
        assert!(wired.contains("for f in \"$HOME\"/.kube/config.d/*.yaml"));
        assert_eq!(Tool::wired_rc(&bash, &wired), None);
        assert_eq!(Tool::wired_rc(&bash, &format!("{DEVSETUP_OPEN}\n")), None);

        let zsh = Machine::new(bash.home.clone(), OsString::new(), "zsh");
        fs::create_dir_all(zsh.home.join(".oh-my-zsh")).unwrap();
        let wired = Tool::wired_rc(&zsh, "plugins=(git)\n").unwrap();
        assert!(wired.starts_with("plugins=(git kubectl)\n") && !wired.contains("alias kgp") && wired.contains("(N)"));
        let devsetup = Tool::wired_rc(&zsh, &format!("plugins=(git)\n{DEVSETUP_OPEN}\n")).unwrap();
        assert!(!devsetup.contains(BLOCK_OPEN) && devsetup.contains("plugins=(git kubectl)"));
        let _ = fs::remove_dir_all(dir);
    }
}
