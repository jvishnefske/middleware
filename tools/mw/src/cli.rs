//! Command-line parsing (pure).
//!
//! [`parse`] turns the arguments after the program name into a [`Cli`]
//! (the [`Command`] and `--dry-run`) or a [`Parsed::Help`] request.
//! Options may appear anywhere; an option that belongs to another command
//! is rejected. Values are kept as strings here and validated by the
//! planner ([`crate::plan`]), which owns the naming rules. [`HELP`] is the
//! usage text.

/// Usage text.
pub const HELP: &str = "\
mw - cluster ops for mw-service binaries (build, load, apply, inspect, trace, robots)

USAGE:
    mw [--dry-run] <COMMAND>

COMMANDS:
    build <bin>...             nix build .#<bin> and .#image-<bin> (docker archive)
                               into target/mw/bin/<bin> and target/mw/images/<bin>.tar
    load <bin>...              Copy each image to every lab node (scp) and import it
                               with `k3s ctr -n k8s.io images import`
    apply <bin>...             Render <bin> --print-manifests and kubectl apply them
        --namespace <NS>           [default: apps]
        --replicas <N>             then scale the Deployment to N
        --tag <TAG>                image tag of localhost/mw/<bin> [default: $MW_IMAGE_TAG or latest]
    zenoh apply <svc>...       Generate one Zenoh password per service, store them only
                               in cluster Secrets, render and apply the router manifests
        --namespace <NS>           [default: apps]
    status [svc]               kubectl get the Deployments, Pods and Services mw applied
        --namespace <NS>
    logs <svc>                 Logs of every pod of <svc>, every container (app and istio-proxy)
        --since <D>                e.g. 30s, 10m, 2h
        --namespace <NS>
    down <bin>...              Delete what apply created (by label, not by file)
        --namespace <NS>
    trace <id>                 Print a span tree from Tempo; <id> is a trace id (32 hex
                               digits) or an x-request-id
        --attr <NAME>              span attribute holding the request id
                                   [default: guid:x-request-id, Envoy's tag]
        --since <D>                search window for a request id [default: 1h]
    robot enroll <name>        Run the lab's `just helm::robot-join <name> <selector>` and
                               install the join token, trust bundle and addresses
        --selector <SEL>           SPIRE workload selector [default: unix:uid:1000]
        --robot-dir <DIR>          write into DIR (the robot VM's shared folder)
        --robot-host <HOST>        or copy to HOST:/var/lib/mw-robot/ over ssh
                                   [default: only target/mw/robot/<name>/]

OPTIONS:
    --dry-run      Print the planned steps; run nothing
    -h, --help     Print this help

ENVIRONMENT:
    MW_REPO, MW_LAB_DIR, MW_LAB_NODES, MW_KUBECONFIG, MW_TEMPO_URL, MW_IMAGE_TAG,
    MW_SSH_USER (see the README section \"Ops with mw\").

Every step runs one program with an argument vector (no shell). The first
failing step stops the run. Exit status is 0 on success, 1 when a step
fails, 2 on a usage or planning error.
";

/// Where `robot enroll` installs the enrollment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobotTarget {
    /// Only the local staging directory `target/mw/robot/<name>/`.
    Staging,
    /// A local directory (the robot VM's shared folder).
    Dir(String),
    /// A host reached over ssh; files go to `/var/lib/mw-robot/`.
    Host(String),
}

/// A command, with its options as given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `build <bin>...`.
    Build {
        /// Binaries.
        bins: Vec<String>,
    },
    /// `load <bin>...`.
    Load {
        /// Binaries.
        bins: Vec<String>,
    },
    /// `apply <bin>...`.
    Apply {
        /// Binaries.
        bins: Vec<String>,
        /// Namespace, when given.
        namespace: Option<String>,
        /// Replica count, when given.
        replicas: Option<u32>,
        /// Image tag, when given.
        tag: Option<String>,
    },
    /// `zenoh apply <svc>...`.
    ZenohApply {
        /// Services that get credentials.
        services: Vec<String>,
        /// Namespace, when given.
        namespace: Option<String>,
    },
    /// `status [svc]`.
    Status {
        /// One service, or all of them.
        service: Option<String>,
        /// Namespace, when given.
        namespace: Option<String>,
    },
    /// `logs <svc>`.
    Logs {
        /// Service.
        service: String,
        /// `--since`, when given.
        since: Option<String>,
        /// Namespace, when given.
        namespace: Option<String>,
    },
    /// `down <bin>...`.
    Down {
        /// Binaries.
        bins: Vec<String>,
        /// Namespace, when given.
        namespace: Option<String>,
    },
    /// `trace <id>`.
    Trace {
        /// Trace id or request id.
        id: String,
        /// `--attr`, when given.
        attr: Option<String>,
        /// `--since`, when given.
        since: Option<String>,
    },
    /// `robot enroll <name>`.
    RobotEnroll {
        /// Robot name.
        name: String,
        /// `--selector`, when given.
        selector: Option<String>,
        /// Where the files go.
        target: RobotTarget,
    },
}

/// Parsed arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    /// Print the plan only.
    pub dry_run: bool,
    /// What to do.
    pub command: Command,
}

/// Result of a successful parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// Run (or dry-run) a command.
    Run(Cli),
    /// Print [`HELP`].
    Help,
}

#[derive(Default)]
struct Options {
    namespace: Option<String>,
    replicas: Option<u32>,
    tag: Option<String>,
    since: Option<String>,
    attr: Option<String>,
    selector: Option<String>,
    robot_dir: Option<String>,
    robot_host: Option<String>,
}

fn value(flag: &str, v: Option<String>) -> Result<String, String> {
    v.filter(|v| !v.starts_with("--"))
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn set<T>(flag: &str, slot: &mut Option<T>, v: T) -> Result<(), String> {
    if slot.replace(v).is_some() {
        Err(format!("{flag} given twice"))
    } else {
        Ok(())
    }
}

/// Parses the arguments (without the program name).
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Parsed, String> {
    let mut args = args.into_iter();
    let mut dry_run = false;
    let mut o = Options::default();
    let mut positional: Vec<String> = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--dry-run" => dry_run = true,
            "--namespace" => set(a.as_str(), &mut o.namespace, value(&a, args.next())?)?,
            "--replicas" => {
                let v = value(&a, args.next())?;
                let n = v
                    .parse()
                    .map_err(|_| format!("--replicas: '{v}' is not a number"))?;
                set(a.as_str(), &mut o.replicas, n)?;
            }
            "--tag" => set(a.as_str(), &mut o.tag, value(&a, args.next())?)?,
            "--since" => set(a.as_str(), &mut o.since, value(&a, args.next())?)?,
            "--attr" => set(a.as_str(), &mut o.attr, value(&a, args.next())?)?,
            "--selector" => set(a.as_str(), &mut o.selector, value(&a, args.next())?)?,
            "--robot-dir" => set(a.as_str(), &mut o.robot_dir, value(&a, args.next())?)?,
            "--robot-host" => set(a.as_str(), &mut o.robot_host, value(&a, args.next())?)?,
            s if s.starts_with('-') => return Err(format!("unknown option '{s}'")),
            _ => positional.push(a),
        }
    }
    let mut pos = positional.into_iter();
    let first = pos.next().ok_or("no command given")?;
    let name = match first.as_str() {
        "zenoh" | "robot" => {
            let sub = pos
                .next()
                .ok_or_else(|| format!("'{first}' needs a subcommand"))?;
            format!("{first} {sub}")
        }
        _ => first,
    };
    let only = |flag: &str, given: bool, cmds: &[&str]| {
        if given && !cmds.contains(&name.as_str()) {
            Err(format!("{flag} applies only to '{}'", cmds.join("', '")))
        } else {
            Ok(())
        }
    };
    only(
        "--namespace",
        o.namespace.is_some(),
        &["apply", "zenoh apply", "status", "logs", "down"],
    )?;
    only("--replicas", o.replicas.is_some(), &["apply"])?;
    only("--tag", o.tag.is_some(), &["apply"])?;
    only("--since", o.since.is_some(), &["logs", "trace"])?;
    only("--attr", o.attr.is_some(), &["trace"])?;
    only("--selector", o.selector.is_some(), &["robot enroll"])?;
    only("--robot-dir", o.robot_dir.is_some(), &["robot enroll"])?;
    only("--robot-host", o.robot_host.is_some(), &["robot enroll"])?;

    let rest: Vec<String> = pos.collect();
    let some = |what: &str| -> Result<Vec<String>, String> {
        if rest.is_empty() {
            Err(format!("'{name}' needs at least one {what}"))
        } else {
            Ok(rest.clone())
        }
    };
    let one = |what: &str| -> Result<String, String> {
        match rest.as_slice() {
            [x] => Ok(x.clone()),
            [] => Err(format!("'{name}' needs a {what}")),
            _ => Err(format!("'{name}' takes one {what}")),
        }
    };
    let command = match name.as_str() {
        "build" => Command::Build { bins: some("bin")? },
        "load" => Command::Load { bins: some("bin")? },
        "apply" => Command::Apply {
            bins: some("bin")?,
            namespace: o.namespace,
            replicas: o.replicas,
            tag: o.tag,
        },
        "zenoh apply" => Command::ZenohApply {
            services: some("service")?,
            namespace: o.namespace,
        },
        "status" => Command::Status {
            service: match rest.as_slice() {
                [] => None,
                [s] => Some(s.clone()),
                _ => return Err("'status' takes at most one service".into()),
            },
            namespace: o.namespace,
        },
        "logs" => Command::Logs {
            service: one("service")?,
            since: o.since,
            namespace: o.namespace,
        },
        "down" => Command::Down {
            bins: some("bin")?,
            namespace: o.namespace,
        },
        "trace" => Command::Trace {
            id: one("trace id or request id")?,
            attr: o.attr,
            since: o.since,
        },
        "robot enroll" => Command::RobotEnroll {
            name: one("robot name")?,
            selector: o.selector,
            target: match (o.robot_dir, o.robot_host) {
                (Some(_), Some(_)) => {
                    return Err("--robot-dir and --robot-host exclude each other".into());
                }
                (Some(d), None) => RobotTarget::Dir(d),
                (None, Some(h)) => RobotTarget::Host(h),
                (None, None) => RobotTarget::Staging,
            },
        },
        other => return Err(format!("unknown command '{other}'")),
    };
    Ok(Parsed::Run(Cli { dry_run, command }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Parsed, String> {
        parse(args.iter().map(|s| (*s).to_owned()))
    }

    fn run(args: &[&str]) -> Cli {
        match p(args) {
            Ok(Parsed::Run(c)) => c,
            other => panic!("{args:?}: {other:?}"),
        }
    }

    #[test]
    fn help_and_errors() {
        assert_eq!(p(&["--help"]), Ok(Parsed::Help));
        assert_eq!(p(&["build", "-h"]), Ok(Parsed::Help));
        assert!(p(&[]).is_err());
        assert!(p(&["build"]).is_err());
        assert!(p(&["frobnicate"]).is_err());
        assert!(p(&["zenoh"]).is_err());
        assert!(p(&["build", "x", "--bogus"]).is_err());
        assert!(p(&["build", "x", "--replicas", "2"]).is_err());
        assert!(p(&["apply", "x", "--replicas", "two"]).is_err());
        assert!(p(&["apply", "x", "--namespace"]).is_err());
        assert!(p(&["apply", "x", "--tag", "a", "--tag", "b"]).is_err());
        assert!(p(&["logs"]).is_err());
        assert!(p(&["logs", "a", "b"]).is_err());
        assert!(p(&["status", "a", "b"]).is_err());
        assert!(
            p(&[
                "robot",
                "enroll",
                "r",
                "--robot-dir",
                "d",
                "--robot-host",
                "h"
            ])
            .is_err()
        );
    }

    #[test]
    fn commands_parse() {
        assert_eq!(
            run(&["--dry-run", "build", "demo-echo", "demo-caller"]),
            Cli {
                dry_run: true,
                command: Command::Build {
                    bins: vec!["demo-echo".into(), "demo-caller".into()]
                }
            }
        );
        assert_eq!(
            run(&[
                "apply",
                "demo-echo",
                "--replicas",
                "3",
                "--namespace",
                "x",
                "--tag",
                "t"
            ])
            .command,
            Command::Apply {
                bins: vec!["demo-echo".into()],
                namespace: Some("x".into()),
                replicas: Some(3),
                tag: Some("t".into())
            }
        );
        assert_eq!(
            run(&["zenoh", "apply", "a", "b"]).command,
            Command::ZenohApply {
                services: vec!["a".into(), "b".into()],
                namespace: None
            }
        );
        assert_eq!(
            run(&["status"]).command,
            Command::Status {
                service: None,
                namespace: None
            }
        );
        assert_eq!(
            run(&["logs", "a", "--since", "10m"]).command,
            Command::Logs {
                service: "a".into(),
                since: Some("10m".into()),
                namespace: None
            }
        );
        assert_eq!(
            run(&["trace", "abc", "--attr", "request.id"]).command,
            Command::Trace {
                id: "abc".into(),
                attr: Some("request.id".into()),
                since: None
            }
        );
        assert_eq!(
            run(&["robot", "enroll", "r1", "--robot-host", "10.0.0.9"]).command,
            Command::RobotEnroll {
                name: "r1".into(),
                selector: None,
                target: RobotTarget::Host("10.0.0.9".into())
            }
        );
        assert_eq!(
            run(&["robot", "enroll", "r1"]).command,
            Command::RobotEnroll {
                name: "r1".into(),
                selector: None,
                target: RobotTarget::Staging
            }
        );
        assert!(!run(&["down", "a"]).dry_run);
    }
}
