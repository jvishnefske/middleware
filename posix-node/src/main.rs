//! The `mw-posix-node` binary: see the crate docs and `app`.
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    mw_posix_node::app::main_with_drivers(&[])
}
