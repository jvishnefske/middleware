//! Validates the shipped payloads against `middleware::wasm_abi`:
//! the WAT templates (compiled here) and the Rust examples (which must be
//! built first with `cargo build --release` in `payloads/`).

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn validate(path: &Path) -> abi_check::Report {
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} (build the examples first: `cargo build --release` in payloads/)",
            path.display()
        )
    });
    let wasm = abi_check::load(&bytes).expect("compiles");
    abi_check::check(&wasm).unwrap_or_else(|e| panic!("{}: {e:#?}", path.display()))
}

fn has(list: &[String], names: &[&str]) -> bool {
    names.iter().all(|n| list.iter().any(|x| x == n))
}

#[test]
fn wat_blink_template() {
    let r = validate(&root().join("templates/blink.wat"));
    assert!(has(&r.exports, &["memory", "init", "tick"]));
    assert!(has(
        &r.imports,
        &["publish", "set_tick", "gpio_output", "gpio_write"]
    ));
}

#[test]
fn wat_echo_template() {
    let r = validate(&root().join("templates/echo.wat"));
    assert!(has(
        &r.exports,
        &["memory", "init", "mw_alloc", "on_message"]
    ));
    assert!(has(&r.imports, &["subscribe", "publish"]));
}

fn release(name: &str) -> PathBuf {
    root()
        .join("target/wasm32-unknown-unknown/release")
        .join(format!("{name}.wasm"))
}

#[test]
fn rust_blink_example() {
    let r = validate(&release("blink"));
    assert!(has(
        &r.exports,
        &["memory", "init", "tick", "mw_alloc", "on_message"]
    ));
    assert!(has(
        &r.imports,
        &["publish", "set_tick", "gpio_output", "gpio_write"]
    ));
}

#[test]
fn rust_echo_example() {
    let r = validate(&release("echo"));
    assert!(has(
        &r.exports,
        &["memory", "init", "tick", "mw_alloc", "on_message"]
    ));
    assert!(has(&r.imports, &["subscribe", "publish", "log"]));
}

// ABI 2: interrupts, timers, SPI and UART DMA.

const ALL_EXPORTS: &[&str] = &["memory", "init", "tick", "mw_alloc", "on_message", "on_irq"];

#[test]
fn wat_button_template() {
    let r = validate(&root().join("templates/button.wat"));
    assert!(has(&r.exports, &["memory", "init", "on_irq"]));
    assert!(has(
        &r.imports,
        &["gpio_input", "gpio_irq", "gpio_write", "publish"]
    ));
}

#[test]
fn wat_timer_template() {
    let r = validate(&root().join("templates/timer.wat"));
    assert!(has(&r.exports, &["memory", "init", "on_irq"]));
    assert!(has(&r.imports, &["timer_start", "gpio_write", "publish"]));
}

/// AssemblyScript templates are compiled by `scripts/check-all.sh` (needs
/// Node); skip quietly when that has not happened.
fn assemblyscript(name: &str) -> Option<abi_check::Report> {
    let path = root().join("target/as").join(format!("{name}.wasm"));
    path.exists().then(|| validate(&path))
}

#[test]
fn as_button_template() {
    if let Some(r) = assemblyscript("button") {
        assert!(has(&r.exports, &["memory", "init", "on_irq"]));
        assert!(has(&r.imports, &["gpio_irq", "irq_overflow", "publish"]));
    }
}

#[test]
fn as_spi_template() {
    if let Some(r) = assemblyscript("spi") {
        assert!(has(&r.exports, &["memory", "init", "on_irq"]));
        assert!(has(
            &r.imports,
            &[
                "spi_open",
                "spi_transfer",
                "dma_read",
                "timer_start",
                "publish"
            ]
        ));
    }
}

#[test]
fn rust_button_example() {
    let r = validate(&release("button"));
    assert!(has(&r.exports, ALL_EXPORTS));
    assert!(has(
        &r.imports,
        &[
            "gpio_input",
            "gpio_irq",
            "gpio_output",
            "gpio_write",
            "irq_overflow",
            "publish"
        ]
    ));
}

#[test]
fn rust_spi_loopback_example() {
    let r = validate(&release("spi_loopback"));
    assert!(has(&r.exports, ALL_EXPORTS));
    assert!(has(
        &r.imports,
        &[
            "spi_open",
            "spi_transfer",
            "dma_read",
            "timer_start",
            "publish"
        ]
    ));
}

#[test]
fn rust_uart_echo_example() {
    let r = validate(&release("uart_echo"));
    assert!(has(&r.exports, ALL_EXPORTS));
    assert!(has(
        &r.imports,
        &["uart_open", "uart_read", "uart_write", "publish"]
    ));
}

#[test]
fn abi1_examples_import_no_abi2_functions() {
    // Unused SDK wrappers are stripped, so ABI 1 payloads still load on
    // ABI 1 firmware.
    for name in ["blink", "echo"] {
        let r = validate(&release(name));
        let abi1 = &middleware::wasm_abi::IMPORTS[..9];
        assert!(
            r.imports.iter().all(|i| abi1.iter().any(|a| a.name == i)),
            "{name}: {:?}",
            r.imports
        );
    }
}
