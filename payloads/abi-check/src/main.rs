//! `abi-check <file.wasm|file.wat>...` — validates payload modules against
//! `middleware::wasm_abi` and prints each module's imports and exports.
//! Exits non-zero if any file fails.

use std::process::ExitCode;

fn main() -> ExitCode {
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: abi-check <file.wasm|file.wat>...");
        return ExitCode::from(2);
    }
    let mut failed = false;
    for f in &files {
        let result = std::fs::read(f)
            .map_err(|e| vec![e.to_string()])
            .and_then(|b| abi_check::load(&b).map_err(|e| vec![e]))
            .and_then(|w| {
                let size = w.len();
                abi_check::check(&w).map(|r| (r, size))
            });
        match result {
            Ok((r, size)) => println!(
                "ok   {f} ({size} bytes)\n     imports mw: {}\n     exports: {}\n     memory pages (initial, max): {:?} of {:?} bytes, initial {:?} bytes",
                r.imports.join(", "),
                r.exports.join(", "),
                r.memory_pages,
                r.memory_page_bytes,
                r.initial_memory_bytes()
            ),
            Err(errs) => {
                failed = true;
                println!("FAIL {f}");
                for e in errs {
                    println!("     {e}");
                }
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
