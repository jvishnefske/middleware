//! Driving a binary's top-level future off the main thread (MS57).
//!
//! A static musl binary cannot reliably measure its own main thread's
//! stack: `stacker::remaining_stack`, which pest (and so json5, and so
//! `zenoh::Config`) consults before every parse, reports too little there,
//! and the parse fails with "stack limit reached" on the first value of
//! the Zenoh configuration. On a thread the process spawns, the bounds come
//! from the thread's own attributes and are exact. So a binary's `main`
//! hands its top-level future to [`block_on_thread`], which runs it with
//! the runtime's `block_on` on one spawned thread with a fixed, generous
//! stack, and waits for it.

use std::future::Future;
use std::io;
use std::thread;

/// Stack of the thread [`block_on_thread`] spawns: the size Linux gives a
/// main thread by default (`ulimit -s`), so a future that ran there fits.
pub const MAIN_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Runs `future` to completion with `runtime.block_on` on one spawned
/// thread whose stack is [`MAIN_STACK_BYTES`], and returns its output.
///
/// The calling thread only waits. An error spawning the thread is returned
/// as is; a panic on it is resumed on the caller, as if `block_on` had
/// been called directly.
pub fn block_on_thread<F>(runtime: &tokio::runtime::Runtime, future: F) -> io::Result<F::Output>
where
    F: Future + Send,
    F::Output: Send,
{
    thread::scope(|scope| {
        let handle = thread::Builder::new()
            .name("main-future".into())
            .stack_size(MAIN_STACK_BYTES)
            .spawn_scoped(scope, || runtime.block_on(future))?;
        match handle.join() {
            Ok(output) => Ok(output),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn ms57_the_future_runs_on_a_spawned_thread_and_returns_its_output() {
        let main = thread::current().id();
        let rt = runtime();
        let (output, ran_on) = block_on_thread(&rt, async { (41 + 1, thread::current().id()) })
            .expect("spawned");
        assert_eq!(output, 42);
        assert_ne!(ran_on, main);
    }

    #[test]
    fn ms57_the_zenoh_configuration_parses_on_that_thread() {
        let rt = runtime();
        let built = block_on_thread(&rt, async {
            middleware::zenoh::SessionConfig::new()
                .connect("tcp/127.0.0.1:7".to_owned())
                .build()
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .expect("spawned");
        assert_eq!(built, Ok(()));
    }

    #[test]
    #[should_panic(expected = "inside")]
    fn ms57_a_panic_on_the_thread_reaches_the_caller() {
        let rt = runtime();
        let _ = block_on_thread(&rt, async { panic!("inside") });
    }
}
