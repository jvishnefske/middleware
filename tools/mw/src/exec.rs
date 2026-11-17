//! The imperative shell: runs planned [`Step`]s.
//!
//! [`execute`] runs the steps in order and stops at the first failure,
//! naming the step. Every process is started with
//! [`std::process::Command`] from its [`Proc`] (program and argument
//! vector, never a shell string); standard error is always inherited, and
//! standard output is inherited unless the step captures it:
//!
//! * [`Step::FetchKubeconfig`] and [`Step::RunBinary`] save it to their
//!   file (the kubeconfig with mode 0600);
//! * [`Step::KubectlApply`] collects the source's output, and only when
//!   the source succeeded writes it to kubectl's standard input. For
//!   [`ManifestSource::ZenohSecrets`] the source is in process: one
//!   [`zenoh::PASSWORD_BYTES`] random draw per service from the OS
//!   (getrandom), rendered by [`zenoh::secrets_manifest`]; the passwords
//!   exist only in memory and in the cluster;
//! * [`Step::HttpGet`] parses curl's output with [`tempo`] and prints span
//!   trees (a search with no match is a failure);
//! * [`Step::RobotJoin`] keeps it for the next [`Step::WriteEnrollment`],
//!   which parses it with [`robot::parse_join`] and writes
//!   [`robot::files`] (directory mode 0700, token 0600).
//!
//! [`Step::PortForward`] starts kubectl in the background, waits up to
//! [`PORT_FORWARD_READY`] for its "Forwarding from" line, and is killed
//! when [`execute`] returns. `--dry-run` never reaches this module: the
//! binary prints [`crate::plan::format_plan`] instead.

use crate::plan::{ManifestSource, Name, Proc, Render, Step, curl};
use crate::{robot, tempo, zenoh};
use std::fmt;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// How long a port-forward may take to report that it listens.
pub const PORT_FORWARD_READY: Duration = Duration::from_secs(20);

/// What went wrong inside a step.
#[derive(Debug)]
pub enum Cause {
    /// The program could not be started.
    Spawn(String, std::io::Error),
    /// It exited unsuccessfully.
    Failed(String, ExitStatus),
    /// A local file could not be written.
    Io(String, std::io::Error),
    /// Tempo's response could not be read.
    Tempo(tempo::TempoError),
    /// The robot-join output could not be used.
    Join(robot::JoinError),
    /// No robot-join output precedes a write.
    NoJoinOutput,
    /// A Tempo search found nothing.
    NoTrace,
    /// The OS random source failed.
    Random(getrandom::Error),
    /// A port-forward did not come up.
    PortForward(String),
    /// A step lacks the process its kind always has (a planner bug).
    Internal(&'static str),
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cause::Spawn(p, e) => write!(f, "cannot start {p}: {e}"),
            Cause::Failed(p, s) => write!(f, "{p} failed ({s})"),
            Cause::Io(p, e) => write!(f, "{p}: {e}"),
            Cause::Tempo(e) => e.fmt(f),
            Cause::Join(e) => e.fmt(f),
            Cause::NoJoinOutput => f.write_str("no robot-join output to write"),
            Cause::NoTrace => f.write_str("Tempo found no trace for that request id in the window"),
            Cause::Random(e) => write!(f, "random source: {e}"),
            Cause::PortForward(m) => write!(f, "port-forward: {m}"),
            Cause::Internal(m) => write!(f, "internal error: {m}"),
        }
    }
}

/// A failed step.
#[derive(Debug)]
pub struct ExecError {
    /// One-based step number.
    pub step: usize,
    /// Number of steps.
    pub steps: usize,
    /// The step as `--dry-run` shows it.
    pub shown: String,
    /// What went wrong.
    pub cause: Cause,
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "step {}/{} failed: {}\n  {}",
            self.step, self.steps, self.cause, self.shown
        )
    }
}

impl std::error::Error for ExecError {}

/// A background process killed when dropped.
struct Background(Child);

impl Drop for Background {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Default)]
struct State {
    background: Vec<Background>,
    join_output: Option<String>,
}

fn command(p: &Proc) -> Command {
    let mut c = Command::new(&p.program);
    c.args(&p.args);
    if let Some(d) = &p.cwd {
        c.current_dir(d);
    }
    c.envs(p.env.iter().map(|(k, v)| (k, v)));
    c
}

fn check(p: &Proc, s: ExitStatus) -> Result<(), Cause> {
    if s.success() {
        Ok(())
    } else {
        Err(Cause::Failed(p.program.clone(), s))
    }
}

fn status(p: &Proc) -> Result<(), Cause> {
    let s = command(p)
        .status()
        .map_err(|e| Cause::Spawn(p.program.clone(), e))?;
    check(p, s)
}

fn capture(p: &Proc) -> Result<Vec<u8>, Cause> {
    let out = command(p)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| Cause::Spawn(p.program.clone(), e))?;
    check(p, out.status)?;
    Ok(out.stdout)
}

fn feed(p: &Proc, input: &[u8]) -> Result<(), Cause> {
    let mut child = command(p)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| Cause::Spawn(p.program.clone(), e))?;
    let written = child
        .stdin
        .take()
        .map(|mut stdin| stdin.write_all(input))
        .unwrap_or(Ok(()));
    let s = child
        .wait()
        .map_err(|e| Cause::Spawn(p.program.clone(), e))?;
    check(p, s)?;
    written.map_err(|e| Cause::Io(format!("{} stdin", p.program), e))
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> Cause + '_ {
    move |e| Cause::Io(path.display().to_string(), e)
}

fn mkdirs(path: &Path) -> Result<(), Cause> {
    match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => std::fs::create_dir_all(d).map_err(io(d)),
        _ => Ok(()),
    }
}

fn write_file(path: &Path, bytes: &[u8], mode: u32) -> Result<(), Cause> {
    mkdirs(path)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .map_err(io(path))?;
    // `mode` applies at creation only; an existing file is tightened too.
    f.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(io(path))?;
    f.write_all(bytes).map_err(io(path))
}

fn one(procs: &[Proc], i: usize) -> Result<&Proc, Cause> {
    procs
        .get(i)
        .ok_or(Cause::Internal("step without its process"))
}

fn zenoh_secrets(ns: &Name, services: &[Name]) -> Result<String, Cause> {
    let mut creds = Vec::with_capacity(services.len());
    for s in services {
        let mut b = [0u8; zenoh::PASSWORD_BYTES];
        getrandom::fill(&mut b).map_err(Cause::Random)?;
        creds.push((s.clone(), zenoh::password(&b)));
    }
    Ok(zenoh::secrets_manifest(ns, &creds))
}

fn port_forward(p: &Proc, state: &mut State) -> Result<(), Cause> {
    let mut child = command(p)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| Cause::Spawn(p.program.clone(), e))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Cause::PortForward("no stdout".into()))?;
    let guard = Background(child);
    let (tx, rx) = mpsc::channel();
    // Drains kubectl's output for the life of the forward, so the pipe
    // never fills; ends when kubectl exits.
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    loop {
        match rx.recv_timeout(PORT_FORWARD_READY) {
            Ok(line) if line.contains("Forwarding from") => break,
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(Cause::PortForward(format!(
                    "not listening after {} s",
                    PORT_FORWARD_READY.as_secs()
                )));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Cause::PortForward("kubectl exited".into()));
            }
        }
    }
    state.background.push(guard);
    Ok(())
}

fn http_get(url: &str, render: &Render) -> Result<(), Cause> {
    let body = capture(&curl(url))?;
    let text = String::from_utf8_lossy(&body);
    match render {
        Render::SpanTree { trace_id } => {
            let spans = tempo::parse_trace(&text).map_err(Cause::Tempo)?;
            print!("{}", tempo::format_tree(trace_id, &spans));
        }
        Render::SearchThenTraces { base } => {
            let ids = tempo::parse_search(&text).map_err(Cause::Tempo)?;
            if ids.is_empty() {
                return Err(Cause::NoTrace);
            }
            for id in ids {
                let body = capture(&curl(&tempo::trace_url(base, &id)))?;
                let spans =
                    tempo::parse_trace(&String::from_utf8_lossy(&body)).map_err(Cause::Tempo)?;
                print!("{}", tempo::format_tree(&id, &spans));
            }
        }
    }
    Ok(())
}

fn write_enrollment(
    name: &Name,
    dir: &Path,
    node_address: &str,
    state: &mut State,
) -> Result<(), Cause> {
    let out = state.join_output.take().ok_or(Cause::NoJoinOutput)?;
    let join = robot::parse_join(&out).map_err(Cause::Join)?;
    std::fs::create_dir_all(dir).map_err(io(dir))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io(dir))?;
    for f in robot::files(name, &join, node_address) {
        write_file(&dir.join(f.name), f.contents.as_bytes(), f.mode)?;
    }
    println!(
        "enrolled robot {name}: {} in {}",
        robot::FILE_NAMES.join(", "),
        dir.display()
    );
    Ok(())
}

fn run_step(step: &Step, state: &mut State) -> Result<(), Cause> {
    let procs = step.procs();
    match step {
        Step::NixBuild { out_link, .. } => {
            mkdirs(out_link)?;
            status(one(&procs, 0)?)
        }
        Step::ScpToNode { .. }
        | Step::SshRun { .. }
        | Step::KubectlGet { .. }
        | Step::KubectlLogs { .. }
        | Step::KubectlDelete { .. }
        | Step::KubectlScale { .. }
        | Step::KubectlRolloutRestart { .. } => status(one(&procs, 0)?),
        Step::FetchKubeconfig { out, .. } => {
            let bytes = capture(one(&procs, 0)?)?;
            write_file(out, &bytes, 0o600)
        }
        Step::RunBinary { stdout_to, .. } => {
            let bytes = capture(one(&procs, 0)?)?;
            write_file(stdout_to, &bytes, 0o644)
        }
        Step::KubectlApply { manifests_from, .. } => match manifests_from {
            ManifestSource::ZenohSecrets {
                namespace,
                services,
            } => feed(
                one(&procs, 0)?,
                zenoh_secrets(namespace, services)?.as_bytes(),
            ),
            ManifestSource::Binary { .. } | ManifestSource::Example { .. } => {
                let manifests = capture(one(&procs, 0)?)?;
                feed(one(&procs, 1)?, &manifests)
            }
        },
        Step::PortForward { .. } => port_forward(one(&procs, 0)?, state),
        Step::HttpGet { url, render } => http_get(url, render),
        Step::RobotJoin { .. } => {
            let out = capture(one(&procs, 0)?)?;
            state.join_output = Some(String::from_utf8_lossy(&out).into_owned());
            Ok(())
        }
        Step::WriteEnrollment {
            name,
            dir,
            node_address,
        } => write_enrollment(name, dir, node_address, state),
    }
}

/// Runs `steps` in order, stopping at the first failure (module docs).
/// Progress goes to standard error as `[i/n] <step>`.
pub fn execute(steps: &[Step]) -> Result<(), ExecError> {
    let mut state = State::default();
    let n = steps.len();
    for (i, step) in steps.iter().enumerate() {
        eprintln!("[{}/{n}] {step}", i + 1);
        run_step(step, &mut state).map_err(|cause| ExecError {
            step: i + 1,
            steps: n,
            shown: step.to_string(),
            cause,
        })?;
    }
    Ok(())
}
