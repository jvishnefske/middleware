//! `wat2wasm IN.wat OUT.wasm`, for scripts/updates-demo.sh (no wabt needed).
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [src, out] = a.as_slice() else {
        eprintln!("usage: wat2wasm IN.wat OUT.wasm");
        std::process::exit(2);
    };
    let wasm = wat::parse_file(src).unwrap_or_else(|e| {
        eprintln!("{src}: {e}");
        std::process::exit(1)
    });
    std::fs::write(out, wasm).expect("write");
}
