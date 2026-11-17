//! `mw`: the cluster ops CLI for `mw-service` binaries.
//!
//! It builds a service's binary and container image with nix, loads the
//! image into the lab's k3s nodes, applies the manifests the binary
//! renders, shows status and logs, deletes what it applied, prints a
//! request's trace from Tempo, provisions Zenoh credentials and enrolls
//! robots with the lab's SPIRE server. The serial firmware tool is
//! `mwctl`; `mw` never opens a serial port.
//!
//! Functional core, imperative shell:
//!
//! * [`cli`] — pure argument parsing into a [`cli::Cli`], and the help
//!   text.
//! * [`env`](mod@env) — the `MW_*` environment as a value ([`env::Env`]), read
//!   through a lookup function.
//! * [`plan`] — [`plan::plan`]: a command and an environment become typed
//!   [`plan::Step`]s with their exact argument vectors; every command's
//!   plan is unit tested.
//! * [`tempo`] — Tempo URLs, response parsing and the span-tree formatter.
//! * [`robot`] — the lab's `robot-join` output parser and the robot's
//!   enrollment files.
//! * [`zenoh`] — the Zenoh credential Secrets.
//! * [`exec`] — the shell: runs steps with `std::process::Command`
//!   (argument vectors, no shell), stops at the first failure.
//!
//! The binary (`src/main.rs`) parses, plans, and either prints the plan
//! (`--dry-run`) or executes it.
#![forbid(unsafe_code)]

pub mod cli;
pub mod env;
pub mod exec;
pub mod plan;
pub mod robot;
pub mod tempo;
pub mod zenoh;
