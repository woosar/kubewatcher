use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Where things live on this machine: the home directory, where programs are looked up
/// (`~/.local/bin` first, then `$PATH`) and which shell's rc file gets the wiring.
#[derive(Clone, Debug)]
pub struct Machine {
    pub home: PathBuf,
    pub search_path: OsString,
    /// Name of the login shell, e.g. `zsh` or `bash`.
    pub shell: String,
}

impl Machine {
    /// The real machine, from `$HOME`, `$PATH` and `$SHELL`; None without a home directory.
    #[cfg(not(test))]
    pub fn from_env() -> Option<Self> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)?;
        let shell = std::env::var("SHELL").unwrap_or_default();
        let shell = Path::new(&shell).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Some(Self::new(home, std::env::var_os("PATH").unwrap_or_default(), &shell))
    }

    /// `path` is the `$PATH` to search after `~/.local/bin`.
    pub fn new(home: PathBuf, path: OsString, shell: &str) -> Self {
        let local_bin = home.join(".local").join("bin");
        let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).filter(|d| *d != local_bin).collect();
        dirs.insert(0, local_bin);
        let search_path = std::env::join_paths(dirs).unwrap_or(path);
        Self { home, search_path, shell: shell.to_string() }
    }

    /// Where kubewatcher installs tools.
    pub fn local_bin(&self) -> PathBuf {
        self.home.join(".local").join("bin")
    }

    /// `~/.kube/config`: the write target for current-context (kubectx/kubens).
    pub fn kube_config(&self) -> PathBuf {
        self.home.join(".kube").join("config")
    }

    /// `~/.kube/config.d`: one kubeconfig file per context.
    pub fn config_d(&self) -> PathBuf {
        self.home.join(".kube").join("config.d")
    }

    pub fn is_zsh(&self) -> bool {
        self.shell == "zsh"
    }

    /// `~/.zshrc` for zsh, `~/.bashrc` for everything else.
    pub fn rc_file(&self) -> PathBuf {
        self.home.join(if self.is_zsh() { ".zshrc" } else { ".bashrc" })
    }

    /// zsh with oh-my-zsh, whose kubectl plugin brings `kgp` and friends.
    pub fn has_oh_my_zsh(&self) -> bool {
        self.is_zsh() && self.home.join(".oh-my-zsh").is_dir()
    }

    /// `path` with the home directory shown as `~`.
    pub fn display(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_bin_is_searched_first_and_paths_follow_the_shell() {
        let path = std::env::join_paths(["/usr/bin", "/h/.local/bin", "/bin"]).unwrap();
        let machine = Machine::new(PathBuf::from("/h"), path, "zsh");
        let dirs: Vec<PathBuf> = std::env::split_paths(&machine.search_path).collect();
        assert_eq!(dirs, [PathBuf::from("/h/.local/bin"), PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
        assert_eq!(machine.rc_file(), PathBuf::from("/h/.zshrc"));
        assert!(!machine.has_oh_my_zsh());
        assert_eq!(machine.display(&machine.config_d()), "~/.kube/config.d");
        assert_eq!(machine.display(Path::new("/etc/x")), "/etc/x");
        assert_eq!(machine.kube_config(), PathBuf::from("/h/.kube/config"));

        let bash = Machine::new(PathBuf::from("/h"), OsString::new(), "bash");
        assert_eq!(bash.rc_file(), PathBuf::from("/h/.bashrc"));
    }
}
