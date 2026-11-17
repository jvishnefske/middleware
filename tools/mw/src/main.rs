//! `mw` binary: parse, plan, then print (`--dry-run`) or execute.
//!
//! The environment is read once ([`mw::env::Env::from_vars`] over the
//! process environment, the current directory, the filesystem's answer for
//! the cached lab kubeconfig and the system clock). Exit status: 0 on
//! success, 1 when a step fails, 2 on a usage or planning error.

use mw::cli::{self, Parsed};
use mw::env::Env;
use mw::{exec, plan};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() -> ExitCode {
    let cli = match cli::parse(std::env::args().skip(1)) {
        Ok(Parsed::Help) => {
            print!("{}", cli::HELP);
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Run(c)) => c,
        Err(e) => {
            eprintln!("mw: {e}\n\n{}", cli::HELP);
            return ExitCode::from(2);
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let env = Env::from_vars(|k| std::env::var(k).ok(), &cwd, Path::exists, now);
    let steps = match plan::plan(&cli.command, &env) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mw: {e}");
            return ExitCode::from(2);
        }
    };
    if cli.dry_run {
        print!("{}", plan::format_plan(&steps));
        return ExitCode::SUCCESS;
    }
    match exec::execute(&steps) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mw: {e}");
            ExitCode::from(1)
        }
    }
}
