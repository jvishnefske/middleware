//! Host-side ABI validator for payload modules.
//!
//! [`check`] runs the full `wasmparser` validator and then compares the
//! module against [`middleware::wasm_abi`], the single source of truth:
//!
//! * every import comes from [`IMPORT_MODULE`] (`"mw"`), names an entry of
//!   [`IMPORTS`], is a function, and has exactly that many `i32` parameters
//!   and an `i32` result iff the entry `returns`;
//! * `memory` is exported as a memory;
//! * the optional exports `init () -> ()`, `tick (i32) -> ()`,
//!   `mw_alloc (i32) -> i32`, `on_message (i32 i32 i32) -> ()` and (ABI 2)
//!   `on_irq (i32 i32 i32 i32) -> ()` have those signatures, and
//!   `on_message` and `mw_alloc` come as a pair;
//! * a module that imports an event source ([`EVENT_SOURCES`]: `gpio_irq`,
//!   `timer_start`, `spi_transfer`, `uart_write`) exports `on_irq`, without
//!   which its interrupts and DMA completions could never be delivered.
//!
//! Other exports (e.g. `__data_end` from rustc) are allowed and listed in the
//! [`Report`]. [`load`] accepts `.wasm` bytes or WAT text. The `abi-check`
//! binary applies this to files given on the command line; the integration
//! tests apply it to the built examples and the WAT templates.

use middleware::wasm_abi::{IMPORT_MODULE, IMPORTS, exports};
use wasmparser::{ExternalKind, FuncType, Parser, Payload, TypeRef, ValType};

/// What a valid module imports and exports.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Imported `"mw"` function names, in module order.
    pub imports: Vec<String>,
    /// All export names, in module order.
    pub exports: Vec<String>,
    /// Linear memory limits in 64 KiB pages: (initial, maximum).
    pub memory_pages: Option<(u64, Option<u64>)>,
}

/// Compiles WAT text, or passes binary `.wasm` through.
pub fn load(bytes: &[u8]) -> Result<Vec<u8>, String> {
    wat::parse_bytes(bytes)
        .map(|c| c.into_owned())
        .map_err(|e| format!("wat: {e}"))
}

fn sig(params: usize, returns: bool) -> (Vec<ValType>, Vec<ValType>) {
    (
        vec![ValType::I32; params],
        if returns { vec![ValType::I32] } else { vec![] },
    )
}

fn matches(ty: &FuncType, params: usize, returns: bool) -> bool {
    let (p, r) = sig(params, returns);
    ty.params() == p.as_slice() && ty.results() == r.as_slice()
}

/// Required signature `(params, returns)` of an ABI function export.
fn export_sig(name: &str) -> Option<(usize, bool)> {
    match name {
        exports::INIT => Some((0, false)),
        exports::TICK => Some((1, false)),
        exports::ALLOC => Some((1, true)),
        exports::ON_MESSAGE => Some((3, false)),
        exports::ON_IRQ => Some((4, false)),
        _ => None,
    }
}

/// ABI 2 imports whose effects arrive as `on_irq` events (GPIO edges, timer
/// alarms, DMA completions). A module importing any of them must export
/// `on_irq`.
pub const EVENT_SOURCES: &[&str] = &["gpio_irq", "timer_start", "spi_transfer", "uart_write"];

/// Validates `wasm` (binary) against the ABI. Returns every violation found.
pub fn check(wasm: &[u8]) -> Result<Report, Vec<String>> {
    wasmparser::validate(wasm).map_err(|e| vec![format!("invalid module: {e}")])?;

    let mut errors = Vec::new();
    let mut report = Report::default();
    let mut types: Vec<FuncType> = Vec::new();
    // Type index of every function, imports first.
    let mut funcs: Vec<u32> = Vec::new();
    let mut export_list: Vec<(String, ExternalKind, u32)> = Vec::new();

    for payload in Parser::new(0).parse_all(wasm) {
        let payload = payload.map_err(|e| vec![e.to_string()])?;
        match payload {
            Payload::TypeSection(r) => {
                for t in r.into_iter_err_on_gc_types() {
                    types.push(t.map_err(|e| vec![e.to_string()])?);
                }
            }
            Payload::ImportSection(r) => {
                for imp in r.into_imports() {
                    let imp = imp.map_err(|e| vec![e.to_string()])?;
                    let what = format!("import {}.{}", imp.module, imp.name);
                    if imp.module != IMPORT_MODULE {
                        errors.push(format!("{what}: module must be \"{IMPORT_MODULE}\""));
                    }
                    let TypeRef::Func(ti) = imp.ty else {
                        errors.push(format!("{what}: only functions may be imported"));
                        continue;
                    };
                    funcs.push(ti);
                    match IMPORTS.iter().find(|i| i.name == imp.name) {
                        None => errors.push(format!("{what}: not in wasm_abi::IMPORTS")),
                        Some(spec) => {
                            let ok = types
                                .get(ti as usize)
                                .is_some_and(|t| matches(t, spec.params, spec.returns));
                            if !ok {
                                errors.push(format!(
                                    "{what}: signature must be {} x i32 -> {}",
                                    spec.params,
                                    if spec.returns { "i32" } else { "()" }
                                ));
                            }
                        }
                    }
                    report.imports.push(imp.name.to_string());
                }
            }
            Payload::MemorySection(r) => {
                for m in r {
                    let m = m.map_err(|e| vec![e.to_string()])?;
                    report.memory_pages = Some((m.initial, m.maximum));
                }
            }
            Payload::FunctionSection(r) => {
                for f in r {
                    funcs.push(f.map_err(|e| vec![e.to_string()])?);
                }
            }
            Payload::ExportSection(r) => {
                for e in r {
                    let e = e.map_err(|e| vec![e.to_string()])?;
                    export_list.push((e.name.to_string(), e.kind, e.index));
                }
            }
            _ => {}
        }
    }

    let has = |n: &str| export_list.iter().any(|(name, _, _)| name == n);
    match export_list.iter().find(|(n, _, _)| n == exports::MEMORY) {
        Some((_, ExternalKind::Memory, _)) => {}
        Some(_) => errors.push("export memory: must be a memory".into()),
        None => errors.push("export memory: missing".into()),
    }
    for (name, kind, index) in &export_list {
        report.exports.push(name.clone());
        let Some((params, returns)) = export_sig(name) else {
            continue;
        };
        let ok = *kind == ExternalKind::Func
            && funcs
                .get(*index as usize)
                .and_then(|ti| types.get(*ti as usize))
                .is_some_and(|t| matches(t, params, returns));
        if !ok {
            errors.push(format!(
                "export {name}: must be a function {params} x i32 -> {}",
                if returns { "i32" } else { "()" }
            ));
        }
    }
    if has(exports::ON_MESSAGE) != has(exports::ALLOC) {
        errors.push("exports on_message and mw_alloc must both be present or both absent".into());
    }
    if !has(exports::ON_IRQ) {
        for source in EVENT_SOURCES {
            if report.imports.iter().any(|i| i == source) {
                errors.push(format!(
                    "import mw.{source}: requires the on_irq export to receive its events"
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(report)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(wat: &str) -> Result<Report, Vec<String>> {
        check(&load(wat.as_bytes()).expect("wat"))
    }

    #[test]
    fn accepts_minimal_module() {
        let r = run(r#"(module (memory (export "memory") 1))"#).expect("valid");
        assert_eq!(r.exports, ["memory"]);
    }

    #[test]
    fn rejects_foreign_module_unknown_name_and_bad_signature() {
        let errs = run(r#"(module
            (import "env" "log" (func (param i32 i32)))
            (import "mw" "printf" (func))
            (import "mw" "now_ms" (func (param i32) (result i32)))
            (memory (export "memory") 1))"#)
        .expect_err("invalid");
        assert_eq!(errs.len(), 3, "{errs:?}");
    }

    #[test]
    fn rejects_missing_memory_and_bad_exports() {
        let errs = run(r#"(module
            (func (export "tick") (result i32) i32.const 0)
            (func (export "on_message") (param i32 i32 i32)))"#)
        .expect_err("invalid");
        assert!(
            errs.iter().any(|e| e.contains("memory: missing")),
            "{errs:?}"
        );
        assert!(errs.iter().any(|e| e.contains("export tick")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("both")), "{errs:?}");
    }

    #[test]
    fn accepts_abi2_imports_with_on_irq() {
        let r = run(r#"(module
            (import "mw" "gpio_irq" (func (param i32 i32) (result i32)))
            (import "mw" "spi_open" (func (param i32 i32 i32 i32 i32 i32 i32) (result i32)))
            (import "mw" "dma_read" (func (param i32 i32 i32) (result i32)))
            (import "mw" "now_us" (func (result i32)))
            (memory (export "memory") 1 1)
            (func (export "on_irq") (param i32 i32 i32 i32)))"#)
        .expect("valid");
        assert!(r.exports.iter().any(|e| e == "on_irq"));
        assert_eq!(r.imports.len(), 4);
    }

    #[test]
    fn rejects_bad_on_irq_signature() {
        let errs = run(r#"(module
            (memory (export "memory") 1)
            (func (export "on_irq") (param i32 i32 i32)))"#)
        .expect_err("invalid");
        assert!(
            errs.iter()
                .any(|e| e.contains("export on_irq: must be a function 4 x i32")),
            "{errs:?}"
        );
    }

    #[test]
    fn event_sources_require_on_irq() {
        let errs = run(r#"(module
            (import "mw" "timer_start" (func (param i32 i32 i32) (result i32)))
            (import "mw" "uart_write" (func (param i32 i32 i32) (result i32)))
            (import "mw" "uart_read" (func (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1))"#)
        .expect_err("invalid");
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(errs[0].contains("timer_start") && errs[1].contains("uart_write"));
        // Polling UART receive alone needs no on_irq.
        run(r#"(module
            (import "mw" "uart_open" (func (param i32 i32 i32 i32) (result i32)))
            (import "mw" "uart_read" (func (param i32 i32 i32) (result i32)))
            (memory (export "memory") 1))"#)
        .expect("valid");
    }

    #[test]
    fn event_sources_are_abi_imports() {
        for s in EVENT_SOURCES {
            assert!(middleware::wasm_abi::import(s).is_some(), "{s}");
        }
    }
}
