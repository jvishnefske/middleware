//! The `mw-posix-node` binary: see the crate docs and `app`.
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    // `mw-posix-node trust pending|accept <fingerprint>`: the SSH claim's
    // second step, as root on the machine (mw_posix_node::provision).
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("trust") {
        let state = std::env::var_os("MW_POSIX_NODE_STATE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| mw_posix_node::config::DEFAULT_STATE.into());
        return mw_posix_node::provision::cli(&state, &args[1..]);
    }
    mw_posix_node::app::main_with_drivers(&[])
}
