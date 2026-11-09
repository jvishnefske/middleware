//! Where things are: the planner's view of the environment (pure).
//!
//! [`Env::from_vars`] reads the `MW_*` variables through a lookup
//! function, so the shell passes `std::env::var` and tests pass a table.
//! Facts that need the filesystem or a clock (whether the cached lab
//! kubeconfig exists, the current time) are passed in by the shell, so
//! nothing here touches the system.
//!
//! | variable | meaning | default |
//! |----------|---------|---------|
//! | `MW_REPO` | repository root: `nix build` runs here, `target/mw/` lives here | the current directory |
//! | `MW_LAB_DIR` | lab repository root (holds the `justfile` with the `helm::` module and `envs/helm/nix/run-local-kvm.sh`) | `$HOME/src/k8s-logstash-logging` |
//! | `MW_LAB_NODES` | comma-separated node addresses, server first | `192.168.66.11,192.168.66.12` |
//! | `MW_KUBECONFIG` | kubeconfig for every `kubectl` step | `target/mw/lab.yaml`, fetched once with `run-local-kvm.sh kubeconfig` |
//! | `MW_TEMPO_URL` | Tempo's HTTP API base URL | a `kubectl port-forward` to `svc/tempo` in `observability` |
//! | `MW_IMAGE_TAG` | image tag `mw apply` renders | `latest` |
//! | `MW_SSH_USER` | SSH user on lab nodes and robots | `root` |

use std::path::{Path, PathBuf};

/// Default lab nodes: the k3s server, then the agent (lab contract).
pub const DEFAULT_NODES: [&str; 2] = ["192.168.66.11", "192.168.66.12"];
/// Default image tag.
pub const DEFAULT_TAG: &str = "latest";
/// Default namespace for services and zenohd (lab contract: meshed `apps`).
pub const DEFAULT_NAMESPACE: &str = "apps";
/// The lab kubeconfig cache, relative to the repository root.
pub const LAB_KUBECONFIG: &str = "target/mw/lab.yaml";
/// The lab's VM script, relative to the lab repository root.
pub const LAB_SCRIPT: &str = "envs/helm/nix/run-local-kvm.sh";
/// The lab repository, relative to `$HOME`.
pub const DEFAULT_LAB_DIR: &str = "src/k8s-logstash-logging";

/// Which kubeconfig `kubectl` steps use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kubeconfig {
    /// `MW_KUBECONFIG`, used as given.
    Explicit(PathBuf),
    /// The cached lab kubeconfig, already present.
    Cached(PathBuf),
    /// The cached lab kubeconfig is missing: the plan fetches it first.
    Fetch(PathBuf),
}

impl Kubeconfig {
    /// The file `kubectl --kubeconfig` names.
    pub fn path(&self) -> &Path {
        match self {
            Kubeconfig::Explicit(p) | Kubeconfig::Cached(p) | Kubeconfig::Fetch(p) => p,
        }
    }
}

/// Everything the planner needs to know about the outside world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Env {
    /// Repository root (absolute).
    pub repo: PathBuf,
    /// Lab repository root.
    pub lab_dir: PathBuf,
    /// Lab node addresses, server first.
    pub nodes: Vec<String>,
    /// Kubeconfig selection.
    pub kubeconfig: Kubeconfig,
    /// Tempo base URL, when given; otherwise the plan port-forwards.
    pub tempo_url: Option<String>,
    /// Image tag for `mw apply`.
    pub tag: String,
    /// SSH user.
    pub ssh_user: String,
    /// Current Unix time in seconds (the Tempo search window ends here).
    pub now_unix: u64,
}

impl Env {
    /// Builds the environment from variable lookups (module docs).
    /// `cwd` is the default repository root and `exists` answers whether a
    /// path exists (the shell passes `Path::exists`).
    pub fn from_vars(
        var: impl Fn(&str) -> Option<String>,
        cwd: &Path,
        exists: impl Fn(&Path) -> bool,
        now_unix: u64,
    ) -> Env {
        let get = |k: &str| var(k).filter(|v| !v.trim().is_empty());
        let repo = get("MW_REPO").map_or_else(|| cwd.to_path_buf(), PathBuf::from);
        let lab_dir = get("MW_LAB_DIR").map_or_else(
            || {
                get("HOME")
                    .map_or_else(|| PathBuf::from("/root"), PathBuf::from)
                    .join(DEFAULT_LAB_DIR)
            },
            PathBuf::from,
        );
        let nodes = get("MW_LAB_NODES").map_or_else(
            || DEFAULT_NODES.iter().map(|s| (*s).to_owned()).collect(),
            |v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            },
        );
        let kubeconfig = match get("MW_KUBECONFIG") {
            Some(p) => Kubeconfig::Explicit(PathBuf::from(p)),
            None => {
                let cached = repo.join(LAB_KUBECONFIG);
                if exists(&cached) {
                    Kubeconfig::Cached(cached)
                } else {
                    Kubeconfig::Fetch(cached)
                }
            }
        };
        Env {
            repo,
            lab_dir,
            nodes,
            kubeconfig,
            tempo_url: get("MW_TEMPO_URL").map(|u| u.trim_end_matches('/').to_owned()),
            tag: get("MW_IMAGE_TAG").unwrap_or_else(|| DEFAULT_TAG.to_owned()),
            ssh_user: get("MW_SSH_USER").unwrap_or_else(|| "root".to_owned()),
            now_unix,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)], cached: bool) -> Env {
        let table: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Env::from_vars(
            |k| table.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()),
            Path::new("/work"),
            |_| cached,
            1_000,
        )
    }

    #[test]
    fn defaults_follow_the_lab_contract() {
        let e = env(&[("HOME", "/home/u")], false);
        assert_eq!(e.repo, PathBuf::from("/work"));
        assert_eq!(e.lab_dir, PathBuf::from("/home/u/src/k8s-logstash-logging"));
        assert_eq!(e.nodes, ["192.168.66.11", "192.168.66.12"]);
        assert_eq!(
            e.kubeconfig,
            Kubeconfig::Fetch(PathBuf::from("/work/target/mw/lab.yaml"))
        );
        assert_eq!(e.tempo_url, None);
        assert_eq!(e.tag, "latest");
        assert_eq!(e.ssh_user, "root");
    }

    #[test]
    fn variables_override_defaults() {
        let e = env(
            &[
                ("MW_REPO", "/r"),
                ("MW_LAB_DIR", "/lab"),
                ("MW_LAB_NODES", " 10.0.0.1 , ,10.0.0.2"),
                ("MW_KUBECONFIG", "/k.yaml"),
                ("MW_TEMPO_URL", "http://tempo:3200/"),
                ("MW_IMAGE_TAG", "0.6.0"),
                ("MW_SSH_USER", "admin"),
            ],
            false,
        );
        assert_eq!(e.repo, PathBuf::from("/r"));
        assert_eq!(e.lab_dir, PathBuf::from("/lab"));
        assert_eq!(e.nodes, ["10.0.0.1", "10.0.0.2"]);
        assert_eq!(e.kubeconfig, Kubeconfig::Explicit(PathBuf::from("/k.yaml")));
        assert_eq!(e.tempo_url.as_deref(), Some("http://tempo:3200"));
        assert_eq!(e.tag, "0.6.0");
        assert_eq!(e.ssh_user, "admin");
    }

    #[test]
    fn a_cached_lab_kubeconfig_is_reused() {
        let e = env(&[], true);
        assert_eq!(
            e.kubeconfig,
            Kubeconfig::Cached(PathBuf::from("/work/target/mw/lab.yaml"))
        );
    }
}
