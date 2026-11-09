//! Running a [`Service`]: the command line ([`Command`], parsed purely by
//! [`Command::parse`]), [`Service::execute`] (one command, output to a
//! writer), [`Service::main`] (the process entry point) and
//! [`Service::run`] / [`Service::run_with`] /
//! [`Service::run_with_options`] (serve until shut down).
//!
//! # Command line
//!
//! | arguments | command |
//! |-----------|---------|
//! | (none) | [`Command::Run`]: serve, configured from the environment ([`Config`]) |
//! | `--print-spec` | [`Command::PrintSpec`]: the [`ServiceSpec`](crate::ServiceSpec) as YAML |
//! | `--print-manifests [--namespace NS] [--image REF]` | [`Command::PrintManifests`]: the Kubernetes manifests ([`manifest`]) |
//!
//! `--namespace` defaults to [`DEFAULT_NAMESPACE`]; without `--image` the
//! manifests choose the image name. Anything else is an [`ArgsError`].
//! [`Service::main`] exits 0 on success, 1 when running fails, and 2 for a
//! usage error.
//!
//! # Serving
//!
//! [`Service::run_with`] is the runner (the imperative shell around the
//! pure [`Config`], [`Lifecycle`](crate::Lifecycle) and
//! [`Ctx`](crate::Ctx)); its parts:
//!
//! * [`serve`] — start, the request path (dispatch, one serve task per
//!   kind, concurrency), and drain and stop;
//! * [`calls`] — discovery of called kinds and the per-task links;
//! * [`node`] — the node id sent in `mw-node` and `mw-client`;
//! * [`telemetry`] — JSON logs, OTLP export, one server span per request;
//! * [`zenoh`] — the Zenoh client session, credentials, subscription hooks;
//! * [`options`] — [`RunOptions`].
//!
//! ## Environment and ports
//!
//! The runner reads the variables of [`Config`] (`MW_PORT`,
//! `POD_NAMESPACE`, `POD_NAME`, `OTEL_EXPORTER_OTLP_ENDPOINT`,
//! `MW_ZENOH_ROUTER`, `MW_ZENOH_CREDENTIALS`, `MW_SERVICE_MAP`,
//! `MW_DRAIN_GRACE_MS`), `RUST_LOG` (the log filter, [`Service::run`]
//! only) and `/etc/resolv.conf` (the nameserver, when the service calls
//! anything). It listens on one port, `MW_PORT` or the spec's (default
//! 50051), for the services' gRPC, health and probes alike.

use std::fmt;
use std::future::Future;
use std::io::Write;
use std::process::ExitCode;

use crate::builder::Service;
use crate::config::Config;
use crate::error::ServiceError;
use crate::manifest::{self, ManifestOptions};

pub mod calls;
pub mod node;
pub mod options;
pub mod serve;
pub mod telemetry;
pub mod zenoh;

pub use options::RunOptions;

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
            ArgsError::NotFor { option, command } => {
                write!(f, "{option} is not an option of {command}")
            }
        }
    }
}

impl std::error::Error for ArgsError {}

/// The usage text printed with an [`ArgsError`].
pub const USAGE: &str =
    "usage: <service> [--print-spec | --print-manifests [--namespace NS] [--image REF]]";

impl Command {
    /// Parses the arguments after the program name. Pure.
    pub fn parse<I, S>(args: I) -> Result<Command, ArgsError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut command: Option<&'static str> = None;
        let mut namespace: Option<String> = None;
        let mut image: Option<String> = None;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let arg = arg.as_ref();
            match arg {
                "--print-spec" | "--print-manifests" => {
                    if command.is_some() {
                        return Err(ArgsError::Repeated(arg.to_owned()));
                    }
                    command = Some(if arg == "--print-spec" {
                        "--print-spec"
                    } else {
                        "--print-manifests"
                    });
                }
                "--namespace" | "--image" => {
                    let (name, slot) = if arg == "--namespace" {
                        ("--namespace", &mut namespace)
                    } else {
                        ("--image", &mut image)
                    };
                    let value = args
                        .next()
                        .map(|v| v.as_ref().to_owned())
                        .filter(|v| !v.starts_with("--"))
                        .ok_or(ArgsError::MissingValue(name))?;
                    if slot.replace(value).is_some() {
                        return Err(ArgsError::Repeated(name.to_owned()));
                    }
                }
                other => return Err(ArgsError::Unknown(other.to_owned())),
            }
        }
        let options_given = namespace.is_some() || image.is_some();
        let option = if namespace.is_some() {
            "--namespace"
        } else {
            "--image"
        };
        match command {
            Some("--print-manifests") => Ok(Command::PrintManifests(ManifestOptions {
                namespace: namespace.unwrap_or_else(|| DEFAULT_NAMESPACE.to_owned()),
                image,
            })),
            Some(c) if options_given => Err(ArgsError::NotFor { option, command: c }),
            Some(_) => Ok(Command::PrintSpec),
            None if options_given => Err(ArgsError::NotFor {
                option,
                command: "run",
            }),
            None => Ok(Command::Run),
        }
    }
}

impl Service {
    /// Serves, configured from the process environment
    /// ([`Config::from_process_env`]), until SIGTERM or SIGINT, draining as
    /// [`Lifecycle`](crate::Lifecycle) describes; a second signal while
    /// draining stops at once. Installs the JSON log subscriber on stdout
    /// first ([`telemetry::init_logs`]). `Ok(())` after a clean drain.
    pub fn run(self) -> impl Future<Output = Result<(), ServiceError>> + Send + 'static {
        let config = Config::from_process_env();
        async move {
            telemetry::init_logs();
            let config = config.inspect_err(|e| tracing::error!(error = %e, "environment"))?;
            if let Some(kind) = config.unmapped_calls(self.spec()).first() {
                return Err(ServiceError::Unmapped((*kind).to_owned()));
            }
            // Boxed: the runner's state is large, keep it off the stack.
            Box::pin(serve::run(
                self,
                config,
                RunOptions::default(),
                terminate_signal(),
                terminate_signal(),
            ))
            .await
            .inspect_err(|e| tracing::error!(error = %e, "run failed"))
        }
    }

    /// Serves with `config` until `shutdown` completes, then drains and
    /// stops ([`serve`]), with the default [`RunOptions`]. For embedding
    /// and tests; it installs no log subscriber. Fails at once if a called
    /// kind has no `MW_SERVICE_MAP` entry.
    pub fn run_with<S>(
        self,
        config: Config,
        shutdown: S,
    ) -> impl Future<Output = Result<(), ServiceError>> + Send + 'static
    where
        S: Future<Output = ()> + Send + 'static,
    {
        self.run_with_options(config, RunOptions::default(), shutdown)
    }

    /// [`Service::run_with`] with `options` (a bound listener, a
    /// nameserver, a dialer, the stop bound, no sidecar).
    // Not an `async fn`: the signature promises `Send + 'static`, so the
    // runner can be spawned.
    #[allow(clippy::manual_async_fn)]
    pub fn run_with_options<S>(
        self,
        config: Config,
        options: RunOptions,
        shutdown: S,
    ) -> impl Future<Output = Result<(), ServiceError>> + Send + 'static
    where
        S: Future<Output = ()> + Send + 'static,
    {
        async move {
            if let Some(kind) = config.unmapped_calls(self.spec()).first() {
                return Err(ServiceError::Unmapped((*kind).to_owned()));
            }
            // Boxed: the runner's state is large, keep it off the stack.
            Box::pin(serve::run(
                self,
                config,
                options,
                shutdown,
                std::future::pending(),
            ))
            .await
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
                image: None
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
                image: Some("demo-echo:dev".into())
            }))
        );
        assert_eq!(
            Command::parse(["--image", "x", "--print-manifests"]),
            Ok(Command::PrintManifests(ManifestOptions {
                namespace: DEFAULT_NAMESPACE.into(),
                image: Some("x".into())
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
    async fn m1_run_with_checks_the_service_map_first() {
        let e = service().run_with(Config::default(), async {}).await;
        assert!(
            matches!(e, Err(ServiceError::Unmapped(ref k)) if k == "demo/echo"),
            "{e:?}"
        );
        // Mapped: it serves, and an immediate shutdown with no drain grace
        // stops cleanly.
        let config = Config {
            service_map: crate::config::parse_service_map("demo/echo=demo-echo.apps:grpc").unwrap(),
            drain_grace: Some(std::time::Duration::ZERO),
            ..Config::default()
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let options = RunOptions::new()
            .listener(listener)
            .nameserver("127.0.0.1:9".parse().unwrap());
        let ran = service().run_with_options(config, options, async {}).await;
        assert!(ran.is_ok(), "{ran:?}");
        // The future is Send + 'static: it can be spawned.
        let handle = tokio::spawn(service().run_with(Config::default(), async {}));
        assert!(handle.await.unwrap().is_err());
    }
}
