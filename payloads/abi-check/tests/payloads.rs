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

/// The 1-byte-page build of an example (`scripts/build-byte-pages.sh`), if
/// it has been built.
fn byte_pages(name: &str) -> Option<abi_check::Report> {
    let path = root()
        .join("target/byte-pages/wasm32-unknown-unknown/release")
        .join(format!("{name}.wasm"));
    path.exists().then(|| validate(&path))
}

#[test]
fn rust_examples_with_one_byte_pages() {
    for name in ["blink", "echo"] {
        let Some(r) = byte_pages(name) else {
            eprintln!("{name}: no byte-page build; run scripts/build-byte-pages.sh");
            continue;
        };
        assert_eq!(r.memory_page_bytes, Some(1), "{name}");
        // check-all.sh links them with 16 KiB fixed memory.
        assert_eq!(r.memory_pages, Some((16_384, Some(16_384))), "{name}");
        assert_eq!(r.initial_memory_bytes(), Some(16_384), "{name}");
    }
}

#[test]
fn default_build_uses_one_64k_page() {
    let r = validate(&release("blink"));
    assert_eq!(r.memory_page_bytes, Some(65_536));
    assert_eq!(r.initial_memory_bytes(), Some(65_536));
}

#[test]
fn wat_one_byte_page_module_validates() {
    let wasm = abi_check::load(br#"(module (memory (export "memory") 300 (pagesize 1)))"#).unwrap();
    let r = abi_check::check(&wasm).expect("custom page sizes are accepted");
    assert_eq!(r.memory_page_bytes, Some(1));
    assert_eq!(r.initial_memory_bytes(), Some(300));
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
fn rust_dimmer_example() {
    let r = validate(&release("dimmer"));
    assert!(has(&r.exports, ALL_EXPORTS));
    assert!(has(
        &r.imports,
        &[
            "pwm_open",
            "pwm_set",
            "adc_read",
            "i2c_open",
            "i2c_transfer",
            "publish"
        ]
    ));
}

// ABI 4: DMA routes.

#[test]
fn rust_cam_stream_example() {
    let r = validate(&release("cam_stream"));
    assert!(has(&r.exports, ALL_EXPORTS));
    assert!(has(&r.imports, &["route_source", "set_tick", "publish"]));
}

#[test]
fn rust_cam_view_example() {
    let r = validate(&release("cam_view"));
    assert!(has(&r.exports, ALL_EXPORTS));
    assert!(has(
        &r.imports,
        &[
            "route_sink",
            "spi_open",
            "spi_transfer",
            "gpio_output",
            "gpio_write",
            "timer_start",
        ]
    ));
}

#[test]
fn cam_view_fits_the_badge() {
    // conagotchi: 16 KiB fixed memory and a 16 KiB image limit.
    let Some(r) = byte_pages("cam_view") else {
        eprintln!("cam_view: no byte-page build; run scripts/build-byte-pages.sh -p cam_view");
        return;
    };
    assert_eq!(r.memory_page_bytes, Some(1));
    assert_eq!(r.initial_memory_bytes(), Some(16_384));
    let path = root().join("target/byte-pages/wasm32-unknown-unknown/release/cam_view.wasm");
    let len = std::fs::metadata(path).expect("built").len();
    assert!(len <= 16 * 1024, "cam_view.wasm is {len} bytes");
}

#[test]
fn rust_cam_jpeg_stream_example() {
    let r = validate(&release("cam_jpeg_stream"));
    assert!(has(&r.exports, ALL_EXPORTS));
    assert!(has(&r.imports, &["route_source", "set_tick", "publish"]));
}

#[test]
fn rust_cam_jpeg_view_example() {
    // Decodes in WASM and draws with its own transfers: no sink route.
    let r = validate(&release("cam_jpeg_view"));
    assert!(has(&r.exports, ALL_EXPORTS));
    assert!(has(
        &r.imports,
        &[
            "subscribe",
            "spi_open",
            "spi_transfer",
            "gpio_output",
            "gpio_write",
            "timer_start",
            "publish",
        ]
    ));
    assert!(
        !r.imports.iter().any(|i| i == "route_sink"),
        "{:?}",
        r.imports
    );
}

#[test]
fn cam_jpeg_view_fits_one_standard_page() {
    // esp-base on the badge: one 64 KiB page, 16 KiB of it stack, frame
    // slots, strip and decoder in .bss (see the crate docs' memory table).
    let r = validate(&release("cam_jpeg_view"));
    assert_eq!(r.memory_pages, Some((1, Some(1))));
    assert_eq!(r.initial_memory_bytes(), Some(65_536));
}

#[test]
fn cam_jpeg_view_deploy_build_declares_exactly_its_memory() {
    // The badge's deploy build (check-all.sh: build-byte-pages.sh -p
    // cam_jpeg_view): 1-byte pages fixed at the crate's
    // [package.metadata.byte-pages] memory, which the host's heap bound
    // charges instead of the 64 KiB cap; and a smaller image than before
    // its panic paths and Scale::Half were dropped (30 878 bytes).
    let Some(r) = byte_pages("cam_jpeg_view") else {
        eprintln!(
            "cam_jpeg_view: no byte-page build; run scripts/build-byte-pages.sh -p cam_jpeg_view"
        );
        return;
    };
    assert_eq!(r.memory_page_bytes, Some(1));
    assert_eq!(r.memory_pages, Some((48_128, Some(48_128))));
    let path = root().join("target/byte-pages/wasm32-unknown-unknown/release/cam_jpeg_view.wasm");
    let len = std::fs::metadata(path).expect("built").len();
    assert!(len <= 26 * 1024, "cam_jpeg_view.wasm is {len} bytes");
}

#[test]
fn p7_sdk_payloads_link_no_panic_formatting() {
    // blink logs through LogLine and receives nothing: with the SDK's
    // panic-free LogLine and delivery path it links no core::fmt panic
    // machinery (5 467 bytes before, about 2 KB now).
    let len = std::fs::metadata(root().join("target/wasm32-unknown-unknown/release/blink.wasm"))
        .expect("built")
        .len();
    assert!(len <= 3 * 1024, "blink.wasm is {len} bytes");
}

#[test]
fn abi3_examples_import_no_abi4_functions() {
    // ABI 3 payloads keep loading on ABI 3 firmware.
    for name in [
        "blink",
        "echo",
        "button",
        "spi_loopback",
        "uart_echo",
        "dimmer",
    ] {
        let r = validate(&release(name));
        let abi3 = &middleware::wasm_abi::IMPORTS[..middleware::wasm_abi::ABI3_IMPORTS];
        assert!(
            r.imports.iter().all(|i| abi3.iter().any(|a| a.name == i)),
            "{name}: {:?}",
            r.imports
        );
    }
}

#[test]
fn abi2_examples_import_no_abi3_functions() {
    // ABI 2 payloads keep loading on ABI 2 firmware.
    for name in ["button", "spi_loopback", "uart_echo"] {
        let r = validate(&release(name));
        let abi2 = &middleware::wasm_abi::IMPORTS[..middleware::wasm_abi::ABI2_IMPORTS];
        assert!(
            r.imports.iter().all(|i| abi2.iter().any(|a| a.name == i)),
            "{name}: {:?}",
            r.imports
        );
    }
}

#[test]
fn abi1_examples_import_no_abi2_functions() {
    // Unused SDK wrappers are stripped, so ABI 1 payloads still load on
    // ABI 1 firmware.
    for name in ["blink", "echo"] {
        let r = validate(&release(name));
        let abi1 = &middleware::wasm_abi::IMPORTS[..middleware::wasm_abi::ABI1_IMPORTS];
        assert!(
            r.imports.iter().all(|i| abi1.iter().any(|a| a.name == i)),
            "{name}: {:?}",
            r.imports
        );
    }
}

// P5: payload crates can forbid unsafe code.

const RUST_EXAMPLES: &[&str] = &[
    "blink",
    "echo",
    "button",
    "spi_loopback",
    "uart_echo",
    "dimmer",
    "cam_stream",
    "cam_view",
    "cam_jpeg_stream",
    "cam_jpeg_view",
];

#[test]
fn p5_forbid_unsafe_code_examples_export_the_abi() {
    for name in RUST_EXAMPLES {
        let src = root().join("examples").join(name).join("src/lib.rs");
        let text = std::fs::read_to_string(&src).expect("example source");
        assert!(
            text.contains("#![forbid(unsafe_code)]"),
            "{name}: must stay #![forbid(unsafe_code)] (it proves payload! needs no allow)"
        );
        assert!(!text.contains("allow(unsafe_code)"), "{name}");
        // It compiled under forbid, and `payload!` still produced every
        // export.
        let r = validate(&release(name));
        assert!(has(&r.exports, ALL_EXPORTS), "{name}: {:?}", r.exports);
    }
}
