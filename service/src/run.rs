//! Running a [`Service`]: the command line ([`Command`], parsed purely by
//! [`Command::parse`]), [`Service::execute`] (one command, output to a
//! writer), [`Service::main`] (the process entry point) and
//! [`Service::run`] / [`Service::run_with`] (serve until shut down).
//!
//! # Command line
//!
//! | arguments | command |
//! |-----------|---------|
//! | (none) | [`Command::Run`]: serve, configured from the environment ([`Config`]) |
//! | `--print-spec` | [`Command::PrintSpec`]: the [`ServiceSpec`](crate::ServiceSpec) as YAML |
//! | `--print-manifests [OPTIONS]` | [`Command::PrintManifests`]: the Kubernetes manifests ([`manifest`]) |
//!
//! The `--print-manifests` options ([`MANIFEST_OPTIONS`]) fill
//! [`ManifestOptions`]; each takes a value and may be given once, except
//! `--route`:
//!
//! | option | field | default |
//! |--------|-------|---------|
//! | `--namespace NS` | `namespace` | [`DEFAULT_NAMESPACE`] |
//! | `--image REF` | `image` | `<name>:latest` |
//! | `--pull-policy P` | `pull_policy` (`Always`, `IfNotPresent`, `Never`) | `IfNotPresent` |
//! | `--otlp URL` | `otlp_endpoint` | the lab's Alloy collector |
//! | `--zenoh-router LOCATOR` | `zenoh_router` | `tcp/zenohd.<ns>.svc.cluster.local:7447` |
//! | `--app-version V` | `app_version` | the image tag |
//! | `--route KIND=SERVICE[.NS]` (repeatable) | `routes` | the callee named after the kind |
//!
//! Anything else is an [`ArgsError`]: an unknown argument, a missing or
//! malformed value, a repeated option, or an option given without
//! `--print-manifests`.
//! [`Service::main`] exits 0 on success, 1 when running fails, and 2 for a
//! usage error.
//!
//! # Status
//!
//! The command line, `--print-spec`, `--print-manifests` and the pure
//! pieces the runner is built from ([`Config`],
//! [`Lifecycle`](crate::Lifecycle), [`Ctx`](crate::Ctx)) are implemented.
//! Serving ([`Service::run_with`]) returns [`ServiceError::Unimplemented`]
//! until its phase lands; nothing panics.

use std::fmt;
use std::future::Future;
use std::io::Write;
use std::process::ExitCode;

use crate::builder::Service;
use crate::config::Config;
use crate::error::ServiceError;
use crate::manifest::{self, ManifestOptions, PullPolicy, Route};

/// The namespace `--print-manifests` renders for without `--namespace`.
pub const DEFAULT_NAMESPACE: &str = "default";

/// What the process was asked to do (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Serve.
    Run,
    /// Print the spec as YAML.
    PrintSpec,
    /// Print the Kubernetes manifests.
    PrintManifests(ManifestOptions),
}

/// A malformed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ArgsError {
    /// An argument no command takes.
    Unknown(String),
    /// An option without its value.
    MissingValue(&'static str),
    /// An option given twice, or two commands.
    Repeated(String),
    /// An option whose value is malformed (`--pull-policy`, `--route`).
    InvalidValue {
        /// The option.
        option: &'static str,
        /// The value given.
        value: String,
    },
    /// An option that belongs to another command.
    NotFor {
        /// The option.
        option: &'static str,
        /// The command it was given with.
        command: &'static str,
    },
}

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArgsError::Unknown(a) => write!(f, "unknown argument {a:?}"),
            ArgsError::MissingValue(o) => write!(f, "{o} needs a value"),
            ArgsError::Repeated(a) => write!(f, "{a} given twice"),
            ArgsError::InvalidValue { option, value } => {
                write!(f, "{option} {value:?} is malformed")
            }
            ArgsError::NotFor { option, command } => {
                write!(f, "{option} is not an option of {command}")
            }
        }
    }
}

impl std::error::Error for ArgsError {}

/// The options of `--print-manifests`, in usage order.
pub const MANIFEST_OPTIONS: [&str; 7] = [
    "--namespace",
    "--image",
    "--pull-policy",
    "--otlp",
    "--zenoh-router",
    "--app-version",
    "--route",
];

/// The usage text printed with an [`ArgsError`].
pub const USAGE: &str = "usage: <service> [--print-spec | --print-manifests [--namespace NS] \
[--image REF] [--pull-policy Always|IfNotPresent|Never] [--otlp URL] [--zenoh-router LOCATOR] \
[--app-version V] [--route KIND=SERVICE[.NS]]...]";

impl Command {
    /// Parses the arguments after the program name. Pure.
    pub fn parse<I, S>(args: I) -> Result<Command, ArgsError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut command: Option<&'static str> = None;
        let mut options = ManifestOptions::new(DEFAULT_NAMESPACE);
        let mut namespace_given = false;
        let mut pull_policy_given = false;
        let mut first_option: Option<&'static str> = None;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let arg = arg.as_ref();
            if let Some(c) = ["--print-spec", "--print-manifests"]
                .into_iter()
                .find(|c| *c == arg)
            {
                if command.replace(c).is_some() {
                    return Err(ArgsError::Repeated(arg.to_owned()));
                }
                continue;
            }
            let Some(option) = MANIFEST_OPTIONS.into_iter().find(|o| *o == arg) else {
                return Err(ArgsError::Unknown(arg.to_owned()));
            };
            first_option.get_or_insert(option);
            let value = args
                .next()
                .map(|v| v.as_ref().to_owned())
                .filter(|v| !v.starts_with("--"))
                .ok_or(ArgsError::MissingValue(option))?;
            let invalid = || ArgsError::InvalidValue {
                option,
                value: value.clone(),
            };
            let repeated = |given: bool| {
                if given {
                    Err(ArgsError::Repeated(option.to_owned()))
                } else {
                    Ok(())
                }
            };
            match option {
                "--namespace" => {
                    repeated(namespace_given)?;
                    namespace_given = true;
                    options.namespace = value;
                }
                "--image" => {
                    repeated(options.image.is_some())?;
                    options.image = Some(value);
                }
                "--pull-policy" => {
                    repeated(pull_policy_given)?;
                    pull_policy_given = true;
                    options.pull_policy = PullPolicy::parse(&value).ok_or_else(invalid)?;
                }
                "--otlp" => {
                    repeated(options.otlp_endpoint.is_some())?;
                    options.otlp_endpoint = Some(value);
                }
                "--zenoh-router" => {
                    repeated(options.zenoh_router.is_some())?;
                    options.zenoh_router = Some(value);
                }
                "--app-version" => {
                    repeated(options.app_version.is_some())?;
                    options.app_version = Some(value);
                }
                _ => {
                    // --route, repeatable (one per kind, checked by render).
                    options
                        .routes
                        .push(Route::parse(&value).ok_or_else(invalid)?);
                }
            }
        }
        match (command, first_option) {
            (Some("--print-manifests"), _) => Ok(Command::PrintManifests(options)),
            (Some(c), Some(option)) => Err(ArgsError::NotFor { option, command: c }),
            (Some(_), None) => Ok(Command::PrintSpec),
            (None, Some(option)) => Err(ArgsError::NotFor {
                option,
                command: "run",
            }),
            (None, None) => Ok(Command::Run),
        }
    }
}

impl Service {
    /// Serves, configured from the process environment
    /// ([`Config::from_process_env`]), until SIGTERM or SIGINT, draining as
    /// [`Lifecycle`](crate::Lifecycle) describes. `Ok(())` after a clean
    /// drain.
    pub fn run(self) -> impl Future<Output = Result<(), ServiceError>> + Send + 'static {
        let config = Config::from_process_env();
        async move {
            let config = config?;
            self.run_with(config, terminate_signal()).await
        }
    }

    /// Serves with `config` until `shutdown` completes (then drains), for
    /// tests and embedding. Not implemented in phase 1:
    /// [`ServiceError::Unimplemented`] after validating that every called
    /// kind is mapped.
    // Not an `async fn`: the signature promises `Send + 'static`, so the
    // runner can be spawned, and later phases must keep that promise.
    #[allow(clippy::manual_async_fn)]
    pub fn run_with<S>(
        self,
        config: Config,
        shutdown: S,
    ) -> impl Future<Output = Result<(), ServiceError>> + Send + 'static
    where
        S: Future<Output = ()> + Send + 'static,
    {
        async move {
            if let Some(kind) = config.unmapped_calls(self.spec()).first() {
                return Err(ServiceError::Unmapped((*kind).to_owned()));
            }
            drop(shutdown);
            Err(ServiceError::Unimplemented("serving (runner, M6)"))
        }
    }

    /// Performs a print command, writing to `out`. [`Command::Run`] is not a
    /// print command: it returns [`ServiceError::Unimplemented`] here (use
    /// [`Service::run`]).
    pub fn execute(&self, command: &Command, out: &mut impl Write) -> Result<(), ServiceError> {
        match command {
            Command::PrintSpec => {
                out.write_all(self.spec().to_yaml()?.as_bytes())?;
                Ok(())
            }
            Command::PrintManifests(options) => {
                out.write_all(manifest::render(self.spec(), options)?.as_bytes())?;
                Ok(())
            }
            Command::Run => Err(ServiceError::Unimplemented(
                "execute(Run): use Service::run",
            )),
        }
    }

    /// The process entry point: parses `std::env::args`, then prints the
    /// spec or manifests, or runs on a multi-threaded tokio runtime. Errors
    /// go to stderr; the exit code is in the module docs.
    pub fn main(self) -> ExitCode {
        let command = match Command::parse(std::env::args().skip(1)) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{e}\n{USAGE}");
                return ExitCode::from(2);
            }
        };
        let result = match command {
            Command::Run => tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(ServiceError::Io)
                .and_then(|rt| rt.block_on(self.run())),
            print => self.execute(&print, &mut std::io::stdout().lock()),
        };
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        }
    }
}

/// Completes on SIGTERM or SIGINT (on non-Unix, on Ctrl-C). If the signal
/// handlers cannot be installed it never completes and the runner's own
/// failure path ends the process.
pub async fn terminate_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(mut term), Ok(mut int)) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = int.recv() => {}
                }
            }
            _ => std::future::pending::<()>().await,
        }
    }
    #[cfg(not(unix))]
    {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Service;

    #[test]
    fn m1_command_line_parses_each_command() {
        assert_eq!(Command::parse::<[&str; 0], &str>([]), Ok(Command::Run));
        assert_eq!(Command::parse(["--print-spec"]), Ok(Command::PrintSpec));
        assert_eq!(
            Command::parse(["--print-manifests"]),
            Ok(Command::PrintManifests(ManifestOptions {
                namespace: DEFAULT_NAMESPACE.into(),
                image: None,
                ..ManifestOptions::default()
            }))
        );
        assert_eq!(
            Command::parse([
                "--print-manifests",
                "--namespace",
                "apps",
                "--image",
                "demo-echo:dev"
            ]),
            Ok(Command::PrintManifests(ManifestOptions {
                namespace: "apps".into(),
                image: Some("demo-echo:dev".into()),
                ..ManifestOptions::default()
            }))
        );
        assert_eq!(
            Command::parse(["--image", "x", "--print-manifests"]),
            Ok(Command::PrintManifests(ManifestOptions {
                namespace: DEFAULT_NAMESPACE.into(),
                image: Some("x".into()),
                ..ManifestOptions::default()
            }))
        );
    }

    #[test]
    fn m1_command_line_errors() {
        assert_eq!(
            Command::parse(["--serve"]),
            Err(ArgsError::Unknown("--serve".into()))
        );
        assert_eq!(
            Command::parse(["--print-manifests", "--namespace"]),
            Err(ArgsError::MissingValue("--namespace"))
        );
        assert_eq!(
            Command::parse(["--print-manifests", "--image", "--namespace"]),
            Err(ArgsError::MissingValue("--image"))
        );
        assert_eq!(
            Command::parse(["--print-spec", "--print-manifests"]),
            Err(ArgsError::Repeated("--print-manifests".into()))
        );
        assert_eq!(
            Command::parse(["--print-manifests", "--image", "a", "--image", "b"]),
            Err(ArgsError::Repeated("--image".into()))
        );
        assert_eq!(
            Command::parse(["--print-spec", "--namespace", "apps"]),
            Err(ArgsError::NotFor {
                option: "--namespace",
                command: "--print-spec"
            })
        );
        assert_eq!(
            Command::parse(["--image", "x"]),
            Err(ArgsError::NotFor {
                option: "--image",
                command: "run"
            })
        );
    }

    #[test]
    fn m30_print_manifests_takes_every_option() {
        let c = Command::parse([
            "--print-manifests",
            "--namespace",
            "apps",
            "--image",
            "demo-caller:dev",
            "--pull-policy",
            "Never",
            "--otlp",
            "http://alloy:4317",
            "--zenoh-router",
            "tcp/zenohd.apps:7447",
            "--app-version",
            "v1",
            "--route",
            "demo/echo=echo",
            "--route",
            "cam/jpeg=cam.robots",
        ]);
        let want = ManifestOptions {
            namespace: "apps".into(),
            image: Some("demo-caller:dev".into()),
            pull_policy: PullPolicy::Never,
            otlp_endpoint: Some("http://alloy:4317".into()),
            zenoh_router: Some("tcp/zenohd.apps:7447".into()),
            app_version: Some("v1".into()),
            routes: vec![
                Route::parse("demo/echo=echo").unwrap(),
                Route::parse("cam/jpeg=cam.robots").unwrap(),
            ],
        };
        assert_eq!(c, Ok(Command::PrintManifests(want)));
        assert_eq!(
            Command::parse(["--print-manifests", "--pull-policy", "never"]),
            Err(ArgsError::InvalidValue {
                option: "--pull-policy",
                value: "never".into()
            })
        );
        assert_eq!(
            Command::parse(["--print-manifests", "--route", "demo/echo"]),
            Err(ArgsError::InvalidValue {
                option: "--route",
                value: "demo/echo".into()
            })
        );
        for option in MANIFEST_OPTIONS.into_iter().filter(|o| *o != "--route") {
            let value = if option == "--pull-policy" {
                "Never"
            } else {
                "x"
            };
            assert_eq!(
                Command::parse(["--print-manifests", option, value, option, value]),
                Err(ArgsError::Repeated(option.into())),
                "{option}"
            );
            assert_eq!(
                Command::parse(["--print-spec", option, value]),
                Err(ArgsError::NotFor {
                    option,
                    command: "--print-spec"
                }),
                "{option}"
            );
        }
        assert!(USAGE.contains("--route KIND=SERVICE[.NS]"));
    }

    #[test]
    fn m30_print_manifests_writes_the_rendered_stream() {
        let options = ManifestOptions::new("apps");
        let mut out = Vec::new();
        service()
            .execute(&Command::PrintManifests(options.clone()), &mut out)
            .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            manifest::render(service().spec(), &options).unwrap()
        );
    }

    fn service() -> Service {
        Service::builder("demo/caller")
            .calls("demo/echo")
            .build()
            .unwrap()
    }

    #[test]
    fn m1_print_spec_writes_the_yaml() {
        let mut out = Vec::new();
        service().execute(&Command::PrintSpec, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, service().spec().to_yaml().unwrap());
        assert!(text.starts_with("name: demo/caller\n"));
        let e = service().execute(&Command::Run, &mut Vec::new());
        assert!(matches!(e, Err(ServiceError::Unimplemented(_))));
    }

    #[tokio::test]
    async fn m1_run_with_checks_the_service_map_then_reports_unimplemented() {
        let e = service().run_with(Config::default(), async {}).await;
        assert!(
            matches!(e, Err(ServiceError::Unmapped(ref k)) if k == "demo/echo"),
            "{e:?}"
        );
        let config = Config {
            service_map: crate::config::parse_service_map("demo/echo=demo-echo.apps:grpc").unwrap(),
            ..Config::default()
        };
        let e = service().run_with(config, async {}).await;
        assert!(matches!(e, Err(ServiceError::Unimplemented(_))), "{e:?}");
        // The future is Send + 'static: it can be spawned.
        let handle = tokio::spawn(service().run_with(Config::default(), async {}));
        assert!(handle.await.unwrap().is_err());
    }
}
