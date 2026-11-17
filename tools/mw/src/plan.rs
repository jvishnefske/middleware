//! Command planning (pure): what each `mw` command does, as data.
//!
//! [`plan`] turns a [`Command`] and an [`Env`] into the [`Step`]s that
//! carry it out, validating every name it puts on a command line first
//! ([`Name`], [`PlanError`]). A step is a typed action; [`Step::procs`]
//! gives the exact processes (program and argument vector, working
//! directory, extra environment) the shell ([`crate::exec`]) runs for it,
//! and its `Display` is what `--dry-run` prints. Nothing here starts a
//! process or touches a file, so every command's plan is unit tested.
//!
//! # Plans
//!
//! | command | steps |
//! |---------|-------|
//! | `build <bin>…` | per bin: [`Step::NixBuild`] of `.#<bin>` → `target/mw/bin/<bin>` and of `.#image-<bin>` → `target/mw/images/<bin>.tar` |
//! | `load <bin>…` | per node and bin: [`Step::ScpToNode`] the tarball to `/tmp/mw-<bin>.tar`, [`Step::SshRun`] `k3s ctr -n k8s.io images import`, then `rm -f` it |
//! | `apply <bin>…` | per bin: [`Step::NixBuild`] `.#<bin>` (a no-op when built), [`Step::KubectlApply`] of `<bin> --print-manifests --namespace NS --image localhost/mw/<bin>:<tag> --pull-policy Never`, and [`Step::KubectlScale`] with `--replicas` |
//! | `zenoh apply <svc>…` | per service: build it and capture `--print-spec` into `target/mw/specs/<svc>.yaml`; apply the generated credential Secrets ([`ManifestSource::ZenohSecrets`]); apply the `zenoh_router_manifests` example's output for those specs; restart zenohd and the mw services so they read the new credentials |
//! | `status [svc]` | [`Step::KubectlGet`] deployments, pods and services by label |
//! | `logs <svc>` | [`Step::KubectlLogs`] by label, all containers (the app and `istio-proxy`), prefixed |
//! | `down <bin>…` | [`Step::KubectlDelete`] of every kind apply creates, by label |
//! | `trace <id>` | [`Step::PortForward`] to Tempo unless `MW_TEMPO_URL` is set, then [`Step::HttpGet`] of `/api/traces/<id>` (trace id) or `/api/search` by request id, rendered as a span tree |
//! | `robot enroll <name>` | [`Step::RobotJoin`] (`just helm::robot-join`), [`Step::WriteEnrollment`], and for `--robot-host` [`Step::SshRun`] `mkdir` plus [`Step::ScpToNode`] of each file |
//!
//! Every plan that runs `kubectl` starts with [`Step::FetchKubeconfig`]
//! when the cached lab kubeconfig is missing ([`Kubeconfig::Fetch`]).
//!
//! Labels: apply's manifests (rendered by `mw-service`) carry
//! `app.kubernetes.io/managed-by=mw-service` and
//! `app.kubernetes.io/name=<bin>`; status, logs, down and the zenoh
//! restart select on them ([`MANAGED_BY`]).

use crate::cli::{Command, RobotTarget};
use crate::env::{Env, Kubeconfig};
use crate::robot;
use crate::tempo;
use crate::zenoh;
use std::fmt;
use std::path::{Path, PathBuf};

/// The label selector every mw-service object carries.
pub const MANAGED_BY: &str = "app.kubernetes.io/managed-by=mw-service";
/// The label holding the service name.
pub const NAME_LABEL: &str = "app.kubernetes.io/name";
/// Image repository prefix: images are `localhost/mw/<bin>:<tag>`.
pub const IMAGE_PREFIX: &str = "localhost/mw/";
/// The kinds `mw down` deletes: everything `--print-manifests` renders,
/// plus the per-service Zenoh credential Secret.
pub const DOWN_KINDS: &str = "deployments.apps,services,serviceaccounts,poddisruptionbudgets.policy,authorizationpolicies.security.istio.io,destinationrules.networking.istio.io,secrets";
/// SSH options for lab nodes and robots: never prompt; the lab VMs are
/// rebuilt with fresh host keys on an isolated bridge, so known_hosts is
/// not consulted (the lab's own `run-local-kvm.sh ssh` does the same).
pub const SSH_OPTS: [&str; 8] = [
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "LogLevel=ERROR",
];
/// Tempo's namespace and Service (lab contract) and its HTTP port.
pub const TEMPO_NAMESPACE: &str = "observability";
/// Tempo's Service.
pub const TEMPO_SERVICE: &str = "svc/tempo";
/// Tempo's HTTP API port.
pub const TEMPO_PORT: u16 = 3200;
/// Local end of the Tempo port-forward.
pub const TEMPO_LOCAL_PORT: u16 = 13200;
/// Default span attribute holding the request id (Envoy's tag).
pub const DEFAULT_REQUEST_ID_ATTR: &str = "guid:x-request-id";
/// Default search window for a request id, in seconds.
pub const DEFAULT_TRACE_WINDOW_S: u64 = 3600;
/// Default SPIRE workload selector for a robot's app.
pub const DEFAULT_SELECTOR: &str = "unix:uid:1000";
/// The zenohd Deployment restarted after new credentials.
pub const ZENOHD_DEPLOYMENT: &str = "deployment/zenohd";
/// The package and example that render the router manifests.
pub const ZENOH_EXAMPLE_PACKAGE: &str = "mw-service";
/// The example that renders the router manifests from service specs.
pub const ZENOH_EXAMPLE: &str = "zenoh_router_manifests";

/// A Kubernetes-style name (RFC 1123 label): 1–63 of `a-z`, `0-9`, `-`,
/// starting and ending with a letter or digit. Binaries, services,
/// namespaces and robots are all `Name`s, so none can be mistaken for an
/// option or reach a remote shell with metacharacters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Name(String);

impl Name {
    /// Validates `s`.
    pub fn parse(what: &'static str, s: &str) -> Result<Name, PlanError> {
        let b = s.as_bytes();
        let ok_char = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-';
        let alnum =
            |c: Option<&u8>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        if (1..=63).contains(&b.len())
            && b.iter().all(ok_char)
            && alnum(b.first())
            && alnum(b.last())
        {
            Ok(Name(s.to_owned()))
        } else {
            Err(PlanError::InvalidName {
                what,
                value: s.to_owned(),
            })
        }
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why no plan could be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// A name is not an RFC 1123 label.
    InvalidName {
        /// What was named (bin, service, namespace, robot).
        what: &'static str,
        /// The value.
        value: String,
    },
    /// The same name twice in one command.
    Duplicate(String),
    /// `--tag` (or `MW_IMAGE_TAG`) is not an image tag.
    InvalidTag(String),
    /// `--since` is not `<n>s`, `<n>m` or `<n>h` with `n > 0`.
    InvalidSince(String),
    /// The trace argument is neither a trace id nor a request id.
    InvalidId(String),
    /// `--attr` is not a usable attribute name.
    InvalidAttr(String),
    /// `--selector` is not `<type>:<value>`.
    InvalidSelector(String),
    /// A host (node or robot) that could be read as an option or a path.
    InvalidHost(String),
    /// `--robot-dir` is empty.
    EmptyRobotDir,
    /// `--replicas 0` (use `mw down`).
    ZeroReplicas,
    /// `MW_LAB_NODES` is empty.
    NoNodes,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::InvalidName { what, value } => write!(
                f,
                "{what} '{value}' is not a valid name (1-63 of a-z, 0-9, '-', starting and ending alphanumeric)"
            ),
            PlanError::Duplicate(n) => write!(f, "'{n}' is named twice"),
            PlanError::InvalidTag(t) => write!(f, "'{t}' is not a valid image tag"),
            PlanError::InvalidSince(s) => {
                write!(
                    f,
                    "--since '{s}': expected a positive number with s, m or h"
                )
            }
            PlanError::InvalidId(s) => write!(
                f,
                "'{s}' is neither a trace id (32 hex digits) nor a request id (1-64 visible ASCII)"
            ),
            PlanError::InvalidAttr(s) => write!(f, "--attr '{s}' is not an attribute name"),
            PlanError::InvalidSelector(s) => {
                write!(
                    f,
                    "--selector '{s}': expected <type>:<value>, e.g. unix:uid:1000"
                )
            }
            PlanError::InvalidHost(h) => write!(f, "'{h}' is not a host name or address"),
            PlanError::EmptyRobotDir => f.write_str("--robot-dir is empty"),
            PlanError::ZeroReplicas => f.write_str("--replicas must be at least 1 (use mw down)"),
            PlanError::NoNodes => f.write_str("MW_LAB_NODES names no node"),
        }
    }
}

impl std::error::Error for PlanError {}

/// An SSH destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// User.
    pub user: String,
    /// Host name or address.
    pub host: String,
}

impl Node {
    fn parse(user: &str, host: &str) -> Result<Node, PlanError> {
        let bad = |s: &str| {
            s.is_empty()
                || s.starts_with('-')
                || s.chars()
                    .any(|c| c.is_whitespace() || matches!(c, '@' | ':' | '/' | '\'' | '"' | ';'))
        };
        if bad(host) {
            return Err(PlanError::InvalidHost(host.to_owned()));
        }
        if bad(user) {
            return Err(PlanError::InvalidHost(format!("{user}@{host}")));
        }
        Ok(Node {
            user: user.to_owned(),
            host: host.to_owned(),
        })
    }

    /// `user@host`.
    pub fn destination(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }
}

/// One process: program, argument vector, working directory, extra
/// environment. Run without a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    /// Program (looked up on `PATH` unless it contains a `/`).
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Working directory, when not inherited.
    pub cwd: Option<PathBuf>,
    /// Variables added to the inherited environment.
    pub env: Vec<(String, String)>,
}

impl Proc {
    fn new<S: Into<String>>(program: impl Into<String>, args: impl IntoIterator<Item = S>) -> Proc {
        Proc {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: None,
            env: Vec::new(),
        }
    }

    fn cwd(self, dir: &Path) -> Proc {
        Proc {
            cwd: Some(dir.to_path_buf()),
            ..self
        }
    }

    fn env(self, k: &str, v: &Path) -> Proc {
        let mut env = self.env;
        env.push((k.to_owned(), v.display().to_string()));
        Proc { env, ..self }
    }

    /// The full argument vector, program first.
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }
}

/// Quotes one word for display only (nothing is ever run through a shell).
fn quote(w: &str) -> String {
    let plain = !w.is_empty()
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=,@+#%".contains(c));
    if plain {
        w.to_owned()
    } else {
        format!("'{}'", w.replace('\'', r"'\''"))
    }
}

impl fmt::Display for Proc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(d) = &self.cwd {
            write!(f, "(cd {}) ", quote(&d.display().to_string()))?;
        }
        for (k, v) in &self.env {
            write!(f, "{k}={} ", quote(v))?;
        }
        let words: Vec<String> = self.argv().iter().map(|w| quote(w)).collect();
        f.write_str(&words.join(" "))
    }
}

/// Where `kubectl apply -f -` reads its manifests from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestSource {
    /// The standard output of a built binary.
    Binary {
        /// Binary path.
        bin: PathBuf,
        /// Its arguments.
        args: Vec<String>,
    },
    /// The standard output of `cargo run -p <package> --example <example>`.
    Example {
        /// Package.
        package: String,
        /// Example.
        example: String,
        /// Arguments after `--`.
        args: Vec<String>,
    },
    /// The Zenoh credential Secrets ([`zenoh::secrets_manifest`]) for
    /// these services, with passwords generated in process when the step
    /// runs. They are never written to disk or printed.
    ZenohSecrets {
        /// Namespace.
        namespace: Name,
        /// Services.
        services: Vec<Name>,
    },
}

/// How [`Step::HttpGet`] shows the response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Render {
    /// A Tempo trace (`/api/traces/<id>`), as a span tree.
    SpanTree {
        /// Trace id shown in the header.
        trace_id: String,
    },
    /// A Tempo search: fetch each matching trace from `base` and print its
    /// span tree.
    SearchThenTraces {
        /// Tempo base URL.
        base: String,
    },
}

/// One action of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `nix build -L .#<attr> --out-link <out_link>` in the repository.
    NixBuild {
        /// Repository root.
        repo: PathBuf,
        /// Flake attribute (`demo-echo`, `image-demo-echo`).
        attr: String,
        /// Out link.
        out_link: PathBuf,
    },
    /// `scp <opts> <local> <user@host>:<remote>`.
    ScpToNode {
        /// Destination.
        node: Node,
        /// Local file.
        local: PathBuf,
        /// Remote path.
        remote: String,
    },
    /// `ssh <opts> <user@host> <argv>`.
    SshRun {
        /// Destination.
        node: Node,
        /// Remote argument vector (validated words only).
        argv: Vec<String>,
    },
    /// `<script> kubeconfig`, standard output saved to `out` (mode 0600).
    FetchKubeconfig {
        /// The lab's `run-local-kvm.sh`.
        script: PathBuf,
        /// The cache file.
        out: PathBuf,
    },
    /// Run a binary, standard output saved to `stdout_to`.
    RunBinary {
        /// Binary.
        bin: PathBuf,
        /// Arguments.
        args: Vec<String>,
        /// Output file.
        stdout_to: PathBuf,
    },
    /// Collect manifests from `manifests_from`, then pipe them to
    /// `kubectl apply -f -`. The source runs to completion first; when it
    /// fails, kubectl is not run.
    KubectlApply {
        /// Kubeconfig.
        kubeconfig: PathBuf,
        /// Namespace.
        namespace: Name,
        /// Manifest source.
        manifests_from: ManifestSource,
    },
    /// `kubectl get <kinds> -l <selector> -o wide`.
    KubectlGet {
        /// Kubeconfig.
        kubeconfig: PathBuf,
        /// Namespace.
        namespace: Name,
        /// Comma-separated kinds.
        kinds: String,
        /// Label selector.
        selector: String,
    },
    /// `kubectl logs -l <selector> --all-containers --prefix`.
    KubectlLogs {
        /// Kubeconfig.
        kubeconfig: PathBuf,
        /// Namespace.
        namespace: Name,
        /// Label selector.
        selector: String,
        /// `--since`.
        since: Option<String>,
    },
    /// `kubectl delete <kinds> -l <selector> --ignore-not-found`.
    KubectlDelete {
        /// Kubeconfig.
        kubeconfig: PathBuf,
        /// Namespace.
        namespace: Name,
        /// Comma-separated kinds.
        kinds: String,
        /// Label selector.
        selector: String,
    },
    /// `kubectl scale deployment/<name> --replicas <n>`.
    KubectlScale {
        /// Kubeconfig.
        kubeconfig: PathBuf,
        /// Namespace.
        namespace: Name,
        /// Deployment.
        deployment: Name,
        /// Replicas.
        replicas: u32,
    },
    /// `kubectl rollout restart <target…>`.
    KubectlRolloutRestart {
        /// Kubeconfig.
        kubeconfig: PathBuf,
        /// Namespace.
        namespace: Name,
        /// What to restart, e.g. `["deployment/zenohd"]` or
        /// `["deployment", "-l", <selector>]`.
        target: Vec<String>,
    },
    /// `kubectl port-forward --address 127.0.0.1 <target> <local>:<remote>`,
    /// kept running in the background until the plan ends.
    PortForward {
        /// Kubeconfig.
        kubeconfig: PathBuf,
        /// Namespace.
        namespace: String,
        /// Target, e.g. `svc/tempo`.
        target: String,
        /// Local port.
        local_port: u16,
        /// Remote port.
        remote_port: u16,
    },
    /// `curl -fsS` of a JSON URL, rendered as `render` says.
    HttpGet {
        /// URL.
        url: String,
        /// Rendering.
        render: Render,
    },
    /// `just helm::robot-join <name> <selector>` in the lab repository with
    /// `KUBECONFIG` set; its output feeds the next [`Step::WriteEnrollment`].
    RobotJoin {
        /// Lab repository root.
        lab_dir: PathBuf,
        /// Kubeconfig.
        kubeconfig: PathBuf,
        /// Robot.
        name: Name,
        /// SPIRE workload selector.
        selector: String,
    },
    /// Parse the preceding robot-join output ([`robot::parse_join`]) and
    /// write the enrollment files ([`robot::files`]) into `dir`.
    WriteEnrollment {
        /// Robot.
        name: Name,
        /// Directory.
        dir: PathBuf,
        /// Address of a lab node (SPIRE server and mesh gateway).
        node_address: String,
    },
}

fn kubectl(kubeconfig: &Path, namespace: &str, rest: &[&str]) -> Proc {
    let mut args = vec![
        "--kubeconfig".to_owned(),
        kubeconfig.display().to_string(),
        "-n".to_owned(),
        namespace.to_owned(),
    ];
    args.extend(rest.iter().map(|s| (*s).to_owned()));
    Proc::new("kubectl", args)
}

impl Step {
    /// The processes this step runs, in order (a [`Step::KubectlApply`]
    /// from a binary or example is the source, then kubectl). Empty for
    /// [`Step::WriteEnrollment`], which runs in process.
    pub fn procs(&self) -> Vec<Proc> {
        match self {
            Step::NixBuild {
                repo,
                attr,
                out_link,
            } => vec![
                Proc::new(
                    "nix",
                    [
                        "build".to_owned(),
                        "-L".to_owned(),
                        format!(".#{attr}"),
                        "--out-link".to_owned(),
                        out_link.display().to_string(),
                    ],
                )
                .cwd(repo),
            ],
            Step::ScpToNode {
                node,
                local,
                remote,
            } => {
                let mut args: Vec<String> = SSH_OPTS.iter().map(|s| (*s).to_owned()).collect();
                args.push(local.display().to_string());
                args.push(format!("{}:{remote}", node.destination()));
                vec![Proc::new("scp", args)]
            }
            Step::SshRun { node, argv } => {
                let mut args: Vec<String> = SSH_OPTS.iter().map(|s| (*s).to_owned()).collect();
                args.push(node.destination());
                args.extend(argv.iter().cloned());
                vec![Proc::new("ssh", args)]
            }
            Step::FetchKubeconfig { script, .. } => {
                vec![Proc::new(script.display().to_string(), ["kubeconfig"])]
            }
            Step::RunBinary { bin, args, .. } => {
                vec![Proc::new(bin.display().to_string(), args.clone())]
            }
            Step::KubectlApply {
                kubeconfig,
                namespace,
                manifests_from,
            } => {
                let apply = kubectl(kubeconfig, namespace.as_str(), &["apply", "-f", "-"]);
                match manifests_from {
                    ManifestSource::Binary { bin, args } => {
                        vec![Proc::new(bin.display().to_string(), args.clone()), apply]
                    }
                    ManifestSource::Example {
                        package,
                        example,
                        args,
                    } => {
                        let mut a = vec![
                            "run".to_owned(),
                            "--quiet".to_owned(),
                            "-p".to_owned(),
                            package.clone(),
                            "--example".to_owned(),
                            example.clone(),
                            "--".to_owned(),
                        ];
                        a.extend(args.iter().cloned());
                        vec![Proc::new("cargo", a), apply]
                    }
                    ManifestSource::ZenohSecrets { .. } => vec![apply],
                }
            }
            Step::KubectlGet {
                kubeconfig,
                namespace,
                kinds,
                selector,
            } => vec![kubectl(
                kubeconfig,
                namespace.as_str(),
                &["get", kinds, "-l", selector, "-o", "wide"],
            )],
            Step::KubectlLogs {
                kubeconfig,
                namespace,
                selector,
                since,
            } => {
                let mut rest = vec![
                    "logs",
                    "-l",
                    selector,
                    "--all-containers=true",
                    "--prefix=true",
                    "--max-log-requests=20",
                ];
                if let Some(s) = since {
                    rest.push("--since");
                    rest.push(s);
                }
                vec![kubectl(kubeconfig, namespace.as_str(), &rest)]
            }
            Step::KubectlDelete {
                kubeconfig,
                namespace,
                kinds,
                selector,
            } => vec![kubectl(
                kubeconfig,
                namespace.as_str(),
                &["delete", kinds, "-l", selector, "--ignore-not-found"],
            )],
            Step::KubectlScale {
                kubeconfig,
                namespace,
                deployment,
                replicas,
            } => vec![kubectl(
                kubeconfig,
                namespace.as_str(),
                &[
                    "scale",
                    &format!("deployment/{deployment}"),
                    "--replicas",
                    &replicas.to_string(),
                ],
            )],
            Step::KubectlRolloutRestart {
                kubeconfig,
                namespace,
                target,
            } => {
                let mut rest = vec!["rollout", "restart"];
                rest.extend(target.iter().map(String::as_str));
                vec![kubectl(kubeconfig, namespace.as_str(), &rest)]
            }
            Step::PortForward {
                kubeconfig,
                namespace,
                target,
                local_port,
                remote_port,
            } => vec![kubectl(
                kubeconfig,
                namespace,
                &[
                    "port-forward",
                    "--address",
                    "127.0.0.1",
                    target,
                    &format!("{local_port}:{remote_port}"),
                ],
            )],
            Step::HttpGet { url, .. } => vec![curl(url)],
            Step::RobotJoin {
                lab_dir,
                kubeconfig,
                name,
                selector,
            } => vec![
                Proc::new(
                    "just",
                    [
                        "helm::robot-join".to_owned(),
                        name.to_string(),
                        selector.clone(),
                    ],
                )
                .cwd(lab_dir)
                .env("KUBECONFIG", kubeconfig),
            ],
            Step::WriteEnrollment { .. } => Vec::new(),
        }
    }
}

/// `curl -fsS --max-time 30 -H 'Accept: application/json' <url>`.
pub fn curl(url: &str) -> Proc {
    Proc::new(
        "curl",
        [
            "-fsS",
            "--max-time",
            "30",
            "-H",
            "Accept: application/json",
            url,
        ],
    )
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let procs = self.procs();
        let line = |f: &mut fmt::Formatter<'_>| -> fmt::Result {
            let parts: Vec<String> = procs.iter().map(ToString::to_string).collect();
            f.write_str(&parts.join(" | "))
        };
        match self {
            Step::FetchKubeconfig { out, .. } | Step::RunBinary { stdout_to: out, .. } => {
                line(f)?;
                write!(f, " > {}", quote(&out.display().to_string()))
            }
            Step::KubectlApply {
                manifests_from: ManifestSource::ZenohSecrets { services, .. },
                ..
            } => {
                let names: Vec<&str> = services.iter().map(Name::as_str).collect();
                write!(
                    f,
                    "<Secrets {} and zenoh-{{{}}} with fresh passwords, never written to disk> | ",
                    zenoh::ROUTER_SECRET,
                    names.join(",")
                )?;
                line(f)
            }
            Step::PortForward { .. } => {
                line(f)?;
                f.write_str(" &  # stopped when the plan ends")
            }
            Step::HttpGet {
                render: Render::SearchThenTraces { base },
                ..
            } => {
                line(f)?;
                write!(
                    f,
                    "  # then GET {} per match",
                    tempo::trace_url(base, "<id>")
                )
            }
            Step::RobotJoin { .. } => {
                line(f)?;
                f.write_str("  # output captured")
            }
            Step::WriteEnrollment { dir, .. } => write!(
                f,
                "write {} from the robot-join output into {}",
                robot::FILE_NAMES.join(", "),
                quote(&dir.display().to_string())
            ),
            _ => line(f),
        }
    }
}

/// What `--dry-run` prints: one numbered line per step.
pub fn format_plan(steps: &[Step]) -> String {
    steps
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{:>2}. {s}\n", i + 1))
        .collect()
}

fn names(what: &'static str, raw: &[String]) -> Result<Vec<Name>, PlanError> {
    let mut out: Vec<Name> = Vec::with_capacity(raw.len());
    for r in raw {
        let n = Name::parse(what, r)?;
        if out.contains(&n) {
            return Err(PlanError::Duplicate(r.clone()));
        }
        out.push(n);
    }
    Ok(out)
}

fn namespace(raw: Option<&String>) -> Result<Name, PlanError> {
    Name::parse(
        "namespace",
        raw.map_or(crate::env::DEFAULT_NAMESPACE, String::as_str),
    )
}

fn tag(t: &str) -> Result<&str, PlanError> {
    let b = t.as_bytes();
    let ok = (1..=128).contains(&b.len())
        && b.first()
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'));
    if ok {
        Ok(t)
    } else {
        Err(PlanError::InvalidTag(t.to_owned()))
    }
}

/// Parses `--since` (`<n>s`, `<n>m`, `<n>h`, `n > 0`) into seconds.
pub fn since_seconds(s: &str) -> Result<u64, PlanError> {
    let bad = || PlanError::InvalidSince(s.to_owned());
    let (digits, unit) = s.split_at(s.len().saturating_sub(1));
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(bad()),
    };
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return Err(bad());
    }
    let n: u64 = digits.parse().map_err(|_| bad())?;
    match n.checked_mul(mult) {
        Some(v) if v > 0 => Ok(v),
        _ => Err(bad()),
    }
}

fn selector_for(name: Option<&Name>) -> String {
    match name {
        Some(n) => format!("{MANAGED_BY},{NAME_LABEL}={n}"),
        None => MANAGED_BY.to_owned(),
    }
}

/// The steps that make the kubeconfig available, then its path.
fn kube(env: &Env) -> (Vec<Step>, PathBuf) {
    let path = env.kubeconfig.path().to_path_buf();
    let pre = match &env.kubeconfig {
        Kubeconfig::Fetch(out) => vec![Step::FetchKubeconfig {
            script: env.lab_dir.join(crate::env::LAB_SCRIPT),
            out: out.clone(),
        }],
        Kubeconfig::Explicit(_) | Kubeconfig::Cached(_) => Vec::new(),
    };
    (pre, path)
}

/// `target/mw/bin/<bin>`: the out link of `nix build .#<bin>`.
pub fn bin_link(repo: &Path, bin: &Name) -> PathBuf {
    repo.join("target/mw/bin").join(bin.as_str())
}

/// The executable inside [`bin_link`].
pub fn bin_exe(repo: &Path, bin: &Name) -> PathBuf {
    bin_link(repo, bin).join("bin").join(bin.as_str())
}

/// `target/mw/images/<bin>.tar`: the out link of `nix build .#image-<bin>`.
pub fn image_link(repo: &Path, bin: &Name) -> PathBuf {
    repo.join("target/mw/images")
        .join(format!("{}.tar", bin.as_str()))
}

/// `target/mw/specs/<svc>.yaml`: a service's captured `--print-spec`.
pub fn spec_file(repo: &Path, svc: &Name) -> PathBuf {
    repo.join("target/mw/specs")
        .join(format!("{}.yaml", svc.as_str()))
}

/// `target/mw/robot/<name>`: the local enrollment staging directory.
pub fn robot_staging(repo: &Path, name: &Name) -> PathBuf {
    repo.join("target/mw/robot").join(name.as_str())
}

fn build_bin(env: &Env, bin: &Name) -> Step {
    Step::NixBuild {
        repo: env.repo.clone(),
        attr: bin.to_string(),
        out_link: bin_link(&env.repo, bin),
    }
}

fn nodes(env: &Env) -> Result<Vec<Node>, PlanError> {
    if env.nodes.is_empty() {
        return Err(PlanError::NoNodes);
    }
    env.nodes
        .iter()
        .map(|h| Node::parse(&env.ssh_user, h))
        .collect()
}

/// Plans `cmd` in `env` (module docs).
pub fn plan(cmd: &Command, env: &Env) -> Result<Vec<Step>, PlanError> {
    match cmd {
        Command::Build { bins } => {
            let bins = names("bin", bins)?;
            Ok(bins
                .iter()
                .flat_map(|b| {
                    [
                        build_bin(env, b),
                        Step::NixBuild {
                            repo: env.repo.clone(),
                            attr: format!("image-{b}"),
                            out_link: image_link(&env.repo, b),
                        },
                    ]
                })
                .collect())
        }
        Command::Load { bins } => {
            let bins = names("bin", bins)?;
            let nodes = nodes(env)?;
            let mut steps = Vec::new();
            for node in &nodes {
                for b in &bins {
                    let remote = format!("/tmp/mw-{b}.tar");
                    steps.push(Step::ScpToNode {
                        node: node.clone(),
                        local: image_link(&env.repo, b),
                        remote: remote.clone(),
                    });
                    steps.push(Step::SshRun {
                        node: node.clone(),
                        argv: ["k3s", "ctr", "-n", "k8s.io", "images", "import", &remote]
                            .map(str::to_owned)
                            .to_vec(),
                    });
                    steps.push(Step::SshRun {
                        node: node.clone(),
                        argv: ["rm", "-f", &remote].map(str::to_owned).to_vec(),
                    });
                }
            }
            Ok(steps)
        }
        Command::Apply {
            bins,
            namespace: ns,
            replicas,
            tag: t,
        } => {
            let bins = names("bin", bins)?;
            let ns = namespace(ns.as_ref())?;
            let t = tag(t.as_deref().unwrap_or(&env.tag))?;
            if *replicas == Some(0) {
                return Err(PlanError::ZeroReplicas);
            }
            let (mut steps, kc) = kube(env);
            for b in &bins {
                steps.push(build_bin(env, b));
                steps.push(Step::KubectlApply {
                    kubeconfig: kc.clone(),
                    namespace: ns.clone(),
                    manifests_from: ManifestSource::Binary {
                        bin: bin_exe(&env.repo, b),
                        args: [
                            "--print-manifests",
                            "--namespace",
                            ns.as_str(),
                            "--image",
                            &format!("{IMAGE_PREFIX}{b}:{t}"),
                            "--pull-policy",
                            "Never",
                        ]
                        .map(str::to_owned)
                        .to_vec(),
                    },
                });
                if let Some(n) = replicas {
                    steps.push(Step::KubectlScale {
                        kubeconfig: kc.clone(),
                        namespace: ns.clone(),
                        deployment: b.clone(),
                        replicas: *n,
                    });
                }
            }
            Ok(steps)
        }
        Command::ZenohApply {
            services,
            namespace: ns,
        } => {
            let services = names("service", services)?;
            let ns = namespace(ns.as_ref())?;
            let (mut steps, kc) = kube(env);
            for s in &services {
                steps.push(build_bin(env, s));
                steps.push(Step::RunBinary {
                    bin: bin_exe(&env.repo, s),
                    args: vec!["--print-spec".to_owned()],
                    stdout_to: spec_file(&env.repo, s),
                });
            }
            steps.push(Step::KubectlApply {
                kubeconfig: kc.clone(),
                namespace: ns.clone(),
                manifests_from: ManifestSource::ZenohSecrets {
                    namespace: ns.clone(),
                    services: services.clone(),
                },
            });
            let mut args = vec!["--namespace".to_owned(), ns.to_string()];
            args.extend(
                services
                    .iter()
                    .map(|s| spec_file(&env.repo, s).display().to_string()),
            );
            steps.push(Step::KubectlApply {
                kubeconfig: kc.clone(),
                namespace: ns.clone(),
                manifests_from: ManifestSource::Example {
                    package: ZENOH_EXAMPLE_PACKAGE.to_owned(),
                    example: ZENOH_EXAMPLE.to_owned(),
                    args,
                },
            });
            steps.push(Step::KubectlRolloutRestart {
                kubeconfig: kc.clone(),
                namespace: ns.clone(),
                target: vec![ZENOHD_DEPLOYMENT.to_owned()],
            });
            steps.push(Step::KubectlRolloutRestart {
                kubeconfig: kc,
                namespace: ns,
                target: vec![
                    "deployment".to_owned(),
                    "-l".to_owned(),
                    MANAGED_BY.to_owned(),
                ],
            });
            Ok(steps)
        }
        Command::Status {
            service,
            namespace: ns,
        } => {
            let svc = service
                .as_deref()
                .map(|s| Name::parse("service", s))
                .transpose()?;
            let ns = namespace(ns.as_ref())?;
            let (mut steps, kc) = kube(env);
            steps.push(Step::KubectlGet {
                kubeconfig: kc,
                namespace: ns,
                kinds: "deployments,pods,services".to_owned(),
                selector: selector_for(svc.as_ref()),
            });
            Ok(steps)
        }
        Command::Logs {
            service,
            since,
            namespace: ns,
        } => {
            let svc = Name::parse("service", service)?;
            if let Some(s) = since {
                since_seconds(s)?;
            }
            let ns = namespace(ns.as_ref())?;
            let (mut steps, kc) = kube(env);
            steps.push(Step::KubectlLogs {
                kubeconfig: kc,
                namespace: ns,
                selector: selector_for(Some(&svc)),
                since: since.clone(),
            });
            Ok(steps)
        }
        Command::Down {
            bins,
            namespace: ns,
        } => {
            let bins = names("bin", bins)?;
            let ns = namespace(ns.as_ref())?;
            let (mut steps, kc) = kube(env);
            for b in &bins {
                steps.push(Step::KubectlDelete {
                    kubeconfig: kc.clone(),
                    namespace: ns.clone(),
                    kinds: DOWN_KINDS.to_owned(),
                    selector: selector_for(Some(b)),
                });
            }
            Ok(steps)
        }
        Command::Trace { id, attr, since } => {
            let query = tempo::Query::parse(id).ok_or_else(|| PlanError::InvalidId(id.clone()))?;
            let attr = attr.as_deref().unwrap_or(DEFAULT_REQUEST_ID_ATTR);
            if attr.is_empty()
                || attr
                    .chars()
                    .any(|c| c.is_whitespace() || c == '=' || c == '"')
            {
                return Err(PlanError::InvalidAttr(attr.to_owned()));
            }
            let window = since
                .as_deref()
                .map(since_seconds)
                .transpose()?
                .unwrap_or(DEFAULT_TRACE_WINDOW_S);
            let mut steps = Vec::new();
            let base = match &env.tempo_url {
                Some(u) => u.clone(),
                None => {
                    let (pre, kc) = kube(env);
                    steps.extend(pre);
                    steps.push(Step::PortForward {
                        kubeconfig: kc,
                        namespace: TEMPO_NAMESPACE.to_owned(),
                        target: TEMPO_SERVICE.to_owned(),
                        local_port: TEMPO_LOCAL_PORT,
                        remote_port: TEMPO_PORT,
                    });
                    format!("http://127.0.0.1:{TEMPO_LOCAL_PORT}")
                }
            };
            steps.push(match query {
                tempo::Query::TraceId(t) => Step::HttpGet {
                    url: tempo::trace_url(&base, &t),
                    render: Render::SpanTree { trace_id: t },
                },
                tempo::Query::RequestId(r) => Step::HttpGet {
                    url: tempo::search_url(
                        &base,
                        attr,
                        &r,
                        env.now_unix.saturating_sub(window),
                        env.now_unix,
                    ),
                    render: Render::SearchThenTraces { base },
                },
            });
            Ok(steps)
        }
        Command::RobotEnroll {
            name,
            selector,
            target,
        } => {
            let name = Name::parse("robot", name)?;
            let selector = selector.as_deref().unwrap_or(DEFAULT_SELECTOR);
            let sel_ok = selector.contains(':')
                && !selector.starts_with('-')
                && !selector.starts_with(':')
                && selector.chars().all(|c| c.is_ascii_graphic());
            if !sel_ok {
                return Err(PlanError::InvalidSelector(selector.to_owned()));
            }
            let node_address = nodes(env)?
                .first()
                .map(|n| n.host.clone())
                .ok_or(PlanError::NoNodes)?;
            let host = match target {
                RobotTarget::Host(h) => Some(Node::parse(&env.ssh_user, h)?),
                _ => None,
            };
            let dir = match target {
                RobotTarget::Dir(d) if d.is_empty() => return Err(PlanError::EmptyRobotDir),
                RobotTarget::Dir(d) => PathBuf::from(d),
                RobotTarget::Staging | RobotTarget::Host(_) => robot_staging(&env.repo, &name),
            };
            let (mut steps, kc) = kube(env);
            steps.push(Step::RobotJoin {
                lab_dir: env.lab_dir.clone(),
                kubeconfig: kc,
                name: name.clone(),
                selector: selector.to_owned(),
            });
            steps.push(Step::WriteEnrollment {
                name,
                dir: dir.clone(),
                node_address,
            });
            if let Some(node) = host {
                steps.push(Step::SshRun {
                    node: node.clone(),
                    argv: ["mkdir", "-p", "-m", "0700", robot::ROBOT_DIR]
                        .map(str::to_owned)
                        .to_vec(),
                });
                for f in robot::FILE_NAMES {
                    steps.push(Step::ScpToNode {
                        node: node.clone(),
                        local: dir.join(f),
                        remote: format!("{}/{f}", robot::ROBOT_DIR),
                    });
                }
            }
            Ok(steps)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{self, Parsed};

    fn env() -> Env {
        Env {
            repo: PathBuf::from("/repo"),
            lab_dir: PathBuf::from("/lab"),
            nodes: vec!["192.168.66.11".into(), "192.168.66.12".into()],
            kubeconfig: Kubeconfig::Cached(PathBuf::from("/repo/target/mw/lab.yaml")),
            tempo_url: None,
            tag: "latest".into(),
            ssh_user: "root".into(),
            now_unix: 10_000,
        }
    }

    fn cmd(args: &[&str]) -> Command {
        match cli::parse(args.iter().map(|s| (*s).to_owned())) {
            Ok(Parsed::Run(c)) => c.command,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    /// The plan as argument vectors, one line per process.
    fn argvs(args: &[&str], env: &Env) -> Vec<String> {
        plan(&cmd(args), env)
            .unwrap()
            .iter()
            .flat_map(|s| {
                let p = s.procs();
                if p.is_empty() {
                    vec![format!("<in process> {s}")]
                } else {
                    p.iter().map(|p| p.argv().join(" ")).collect()
                }
            })
            .collect()
    }

    const KC: &str = "kubectl --kubeconfig /repo/target/mw/lab.yaml -n apps";
    const SSH: &str = "-o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR";

    #[test]
    fn m50_build_plans_binary_and_image_per_bin() {
        assert_eq!(
            argvs(&["build", "demo-echo", "demo-caller"], &env()),
            [
                "nix build -L .#demo-echo --out-link /repo/target/mw/bin/demo-echo",
                "nix build -L .#image-demo-echo --out-link /repo/target/mw/images/demo-echo.tar",
                "nix build -L .#demo-caller --out-link /repo/target/mw/bin/demo-caller",
                "nix build -L .#image-demo-caller --out-link /repo/target/mw/images/demo-caller.tar",
            ]
        );
        let steps = plan(&cmd(&["build", "demo-echo"]), &env()).unwrap();
        assert_eq!(steps[0].procs()[0].cwd, Some(PathBuf::from("/repo")));
    }

    #[test]
    fn m50_load_copies_and_imports_on_every_node() {
        let p = argvs(&["load", "demo-echo"], &env());
        assert_eq!(
            p,
            [
                format!(
                    "scp {SSH} /repo/target/mw/images/demo-echo.tar root@192.168.66.11:/tmp/mw-demo-echo.tar"
                ),
                format!(
                    "ssh {SSH} root@192.168.66.11 k3s ctr -n k8s.io images import /tmp/mw-demo-echo.tar"
                ),
                format!("ssh {SSH} root@192.168.66.11 rm -f /tmp/mw-demo-echo.tar"),
                format!(
                    "scp {SSH} /repo/target/mw/images/demo-echo.tar root@192.168.66.12:/tmp/mw-demo-echo.tar"
                ),
                format!(
                    "ssh {SSH} root@192.168.66.12 k3s ctr -n k8s.io images import /tmp/mw-demo-echo.tar"
                ),
                format!("ssh {SSH} root@192.168.66.12 rm -f /tmp/mw-demo-echo.tar"),
            ]
        );
        let mut e = env();
        e.nodes.clear();
        assert_eq!(plan(&cmd(&["load", "a"]), &e), Err(PlanError::NoNodes));
        e.nodes = vec!["-oProxyCommand=x".into()];
        assert!(matches!(
            plan(&cmd(&["load", "a"]), &e),
            Err(PlanError::InvalidHost(_))
        ));
    }

    #[test]
    fn m50_apply_renders_with_the_local_image_and_scales() {
        let p = argvs(&["apply", "demo-echo", "--replicas", "3"], &env());
        assert_eq!(
            p,
            [
                "nix build -L .#demo-echo --out-link /repo/target/mw/bin/demo-echo".to_owned(),
                "/repo/target/mw/bin/demo-echo/bin/demo-echo --print-manifests --namespace apps --image localhost/mw/demo-echo:latest --pull-policy Never".to_owned(),
                format!("{KC} apply -f -"),
                format!("{KC} scale deployment/demo-echo --replicas 3"),
            ]
        );
        let p = argvs(
            &["apply", "x", "--namespace", "lab", "--tag", "0.6.0"],
            &env(),
        );
        assert!(p[1].ends_with("--namespace lab --image localhost/mw/x:0.6.0 --pull-policy Never"));
        assert!(p[2].contains("-n lab apply -f -"));
        assert_eq!(p.len(), 3);
        assert_eq!(
            plan(&cmd(&["apply", "x", "--replicas", "0"]), &env()),
            Err(PlanError::ZeroReplicas)
        );
        assert!(matches!(
            plan(&cmd(&["apply", "x", "--tag", "a b"]), &env()),
            Err(PlanError::InvalidTag(_))
        ));
    }

    #[test]
    fn m50_a_missing_kubeconfig_is_fetched_first() {
        let mut e = env();
        e.kubeconfig = Kubeconfig::Fetch(PathBuf::from("/repo/target/mw/lab.yaml"));
        let steps = plan(&cmd(&["status"]), &e).unwrap();
        assert_eq!(
            steps[0],
            Step::FetchKubeconfig {
                script: PathBuf::from("/lab/envs/helm/nix/run-local-kvm.sh"),
                out: PathBuf::from("/repo/target/mw/lab.yaml"),
            }
        );
        assert_eq!(
            steps[0].to_string(),
            "/lab/envs/helm/nix/run-local-kvm.sh kubeconfig > /repo/target/mw/lab.yaml"
        );
        e.kubeconfig = Kubeconfig::Explicit(PathBuf::from("/k"));
        let p = argvs(&["status"], &e);
        assert_eq!(p.len(), 1);
        assert!(p[0].starts_with("kubectl --kubeconfig /k -n apps get"));
    }

    #[test]
    fn m50_zenoh_apply_plans_specs_secrets_router_and_restarts() {
        let p = argvs(&["zenoh", "apply", "demo-echo", "demo-caller"], &env());
        assert_eq!(
            p,
            [
                "nix build -L .#demo-echo --out-link /repo/target/mw/bin/demo-echo".to_owned(),
                "/repo/target/mw/bin/demo-echo/bin/demo-echo --print-spec".to_owned(),
                "nix build -L .#demo-caller --out-link /repo/target/mw/bin/demo-caller".to_owned(),
                "/repo/target/mw/bin/demo-caller/bin/demo-caller --print-spec".to_owned(),
                format!("{KC} apply -f -"),
                "cargo run --quiet -p mw-service --example zenoh_router_manifests -- --namespace apps /repo/target/mw/specs/demo-echo.yaml /repo/target/mw/specs/demo-caller.yaml".to_owned(),
                format!("{KC} apply -f -"),
                format!("{KC} rollout restart deployment/zenohd"),
                format!("{KC} rollout restart deployment -l app.kubernetes.io/managed-by=mw-service"),
            ]
        );
        let steps = plan(&cmd(&["zenoh", "apply", "demo-echo"]), &env()).unwrap();
        assert_eq!(
            steps[1].to_string(),
            "/repo/target/mw/bin/demo-echo/bin/demo-echo --print-spec > /repo/target/mw/specs/demo-echo.yaml"
        );
        let shown = steps[2].to_string();
        assert!(
            shown.starts_with("<Secrets zenohd-credentials and zenoh-{demo-echo}"),
            "{shown}"
        );
        assert_eq!(
            plan(&cmd(&["zenoh", "apply", "a", "a"]), &env()),
            Err(PlanError::Duplicate("a".into()))
        );
    }

    #[test]
    fn m50_status_logs_down_select_by_label() {
        assert_eq!(
            argvs(&["status"], &env()),
            [format!(
                "{KC} get deployments,pods,services -l app.kubernetes.io/managed-by=mw-service -o wide"
            )]
        );
        assert_eq!(
            argvs(&["status", "demo-echo"], &env()),
            [format!(
                "{KC} get deployments,pods,services -l app.kubernetes.io/managed-by=mw-service,app.kubernetes.io/name=demo-echo -o wide"
            )]
        );
        assert_eq!(
            argvs(&["logs", "demo-echo", "--since", "10m"], &env()),
            [format!(
                "{KC} logs -l app.kubernetes.io/managed-by=mw-service,app.kubernetes.io/name=demo-echo --all-containers=true --prefix=true --max-log-requests=20 --since 10m"
            )]
        );
        assert!(matches!(
            plan(&cmd(&["logs", "a", "--since", "10d"]), &env()),
            Err(PlanError::InvalidSince(_))
        ));
        assert_eq!(
            argvs(&["down", "demo-echo"], &env()),
            [format!(
                "{KC} delete {DOWN_KINDS} -l app.kubernetes.io/managed-by=mw-service,app.kubernetes.io/name=demo-echo --ignore-not-found"
            )]
        );
    }

    #[test]
    fn m50_trace_by_trace_id_port_forwards_then_fetches() {
        let id = "0af7651916cd43dd8448eb211c80319c";
        let steps = plan(&cmd(&["trace", id]), &env()).unwrap();
        assert_eq!(
            steps[0].procs()[0].argv().join(" "),
            "kubectl --kubeconfig /repo/target/mw/lab.yaml -n observability port-forward --address 127.0.0.1 svc/tempo 13200:3200"
        );
        assert_eq!(
            steps[1],
            Step::HttpGet {
                url: format!("http://127.0.0.1:13200/api/traces/{id}"),
                render: Render::SpanTree {
                    trace_id: id.to_owned()
                },
            }
        );
        assert_eq!(
            steps[1].procs()[0].argv(),
            [
                "curl",
                "-fsS",
                "--max-time",
                "30",
                "-H",
                "Accept: application/json",
                &format!("http://127.0.0.1:13200/api/traces/{id}")
            ]
        );
    }

    #[test]
    fn m50_trace_by_request_id_searches_the_window() {
        let mut e = env();
        e.tempo_url = Some("http://tempo:3200".into());
        let steps = plan(&cmd(&["trace", "2b4c-req", "--since", "2h"]), &e).unwrap();
        assert_eq!(
            steps,
            [Step::HttpGet {
                url: "http://tempo:3200/api/search?tags=guid%3Ax-request-id%3D%222b4c-req%22&start=2800&end=10000&limit=20".into(),
                render: Render::SearchThenTraces {
                    base: "http://tempo:3200".into()
                },
            }]
        );
        assert!(matches!(
            plan(&cmd(&["trace", "a b"]), &e),
            Err(PlanError::InvalidId(_))
        ));
        assert!(matches!(
            plan(&cmd(&["trace", "x", "--attr", "a=b"]), &e),
            Err(PlanError::InvalidAttr(_))
        ));
    }

    #[test]
    fn m50_robot_enroll_joins_writes_and_copies() {
        let p = argvs(
            &["robot", "enroll", "r1", "--robot-host", "192.168.66.20"],
            &env(),
        );
        assert_eq!(
            p,
            [
                "just helm::robot-join r1 unix:uid:1000".to_owned(),
                "<in process> write join-token, bundle.spiffe, enrollment.env from the robot-join output into /repo/target/mw/robot/r1".to_owned(),
                format!("ssh {SSH} root@192.168.66.20 mkdir -p -m 0700 /var/lib/mw-robot"),
                format!("scp {SSH} /repo/target/mw/robot/r1/join-token root@192.168.66.20:/var/lib/mw-robot/join-token"),
                format!("scp {SSH} /repo/target/mw/robot/r1/bundle.spiffe root@192.168.66.20:/var/lib/mw-robot/bundle.spiffe"),
                format!("scp {SSH} /repo/target/mw/robot/r1/enrollment.env root@192.168.66.20:/var/lib/mw-robot/enrollment.env"),
            ]
        );
        let steps = plan(&cmd(&["robot", "enroll", "r1"]), &env()).unwrap();
        let join = &steps[0].procs()[0];
        assert_eq!(join.cwd, Some(PathBuf::from("/lab")));
        assert_eq!(
            join.env,
            [(
                "KUBECONFIG".to_owned(),
                "/repo/target/mw/lab.yaml".to_owned()
            )]
        );
        assert_eq!(
            steps[1],
            Step::WriteEnrollment {
                name: Name::parse("robot", "r1").unwrap(),
                dir: PathBuf::from("/repo/target/mw/robot/r1"),
                node_address: "192.168.66.11".into(),
            }
        );
        assert_eq!(steps.len(), 2);
        let steps = plan(
            &cmd(&[
                "robot",
                "enroll",
                "r1",
                "--robot-dir",
                "/srv/vm/r1",
                "--selector",
                "unix:uid:0",
            ]),
            &env(),
        )
        .unwrap();
        assert_eq!(steps.len(), 2);
        assert!(
            steps[0]
                .to_string()
                .contains("helm::robot-join r1 unix:uid:0")
        );
        assert!(
            matches!(&steps[1], Step::WriteEnrollment { dir, .. } if dir == Path::new("/srv/vm/r1"))
        );
        assert!(matches!(
            plan(
                &cmd(&["robot", "enroll", "r1", "--selector", "uid"]),
                &env()
            ),
            Err(PlanError::InvalidSelector(_))
        ));
        assert!(matches!(
            plan(
                &cmd(&["robot", "enroll", "r1", "--robot-host", "h:/x"]),
                &env()
            ),
            Err(PlanError::InvalidHost(_))
        ));
    }

    #[test]
    fn m50_names_are_validated_before_reaching_argv() {
        for bad in ["", "-rf", "Demo", "a_b", "a-", "a;rm", &"a".repeat(64)] {
            let build = Command::Build {
                bins: vec![bad.to_owned()],
            };
            assert!(
                matches!(plan(&build, &env()), Err(PlanError::InvalidName { .. })),
                "{bad:?}"
            );
            assert!(Name::parse("bin", bad).is_err(), "{bad:?}");
        }
        assert!(Name::parse("bin", "demo-echo2").is_ok());
        assert!(matches!(
            plan(&cmd(&["status", "--namespace", "Apps"]), &env()),
            Err(PlanError::InvalidName {
                what: "namespace",
                ..
            })
        ));
    }

    #[test]
    fn since_parses_units() {
        assert_eq!(since_seconds("30s"), Ok(30));
        assert_eq!(since_seconds("10m"), Ok(600));
        assert_eq!(since_seconds("2h"), Ok(7200));
        for bad in ["", "s", "0m", "-1m", "1d", "1.5h", "99999999999999999999h"] {
            assert!(since_seconds(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn display_quotes_only_for_reading() {
        let p = curl("http://x/a b");
        assert_eq!(
            p.to_string(),
            "curl -fsS --max-time 30 -H 'Accept: application/json' 'http://x/a b'"
        );
    }
}
