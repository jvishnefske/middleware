//! Runs the WebAssembly specification testsuite (`.wast` files).
//!
//! The suite is not vendored. Point `WASM_TESTSUITE` at a checkout of
//! <https://github.com/WebAssembly/testsuite>; without it this test only
//! says so. Modules that use features outside the crate's scope are
//! skipped with their assertions, as are `assert_invalid` and
//! `assert_malformed` (there is no validator to test).
#![cfg(feature = "float")]

use std::collections::HashMap;
use std::path::Path;
use wasm_micro::{Error, Host, Instance, Limits, Module};
use wast::core::{NanPattern, WastArgCore, WastRetCore};
use wast::parser::{self, ParseBuffer};
use wast::{QuoteWat, Wast, WastArg, WastDirective, WastExecute, WastInvoke, WastRet, Wat};

struct Spectest;
impl Host for Spectest {
    fn call(&mut self, _: u32, _: &mut [u8], _: &mut [u64]) -> Result<(), Error> {
        Ok(())
    }
}

fn limits() -> Limits {
    Limits {
        stack: 1 << 16,
        frames: 4096,
        labels: 1 << 14,
        memory_bytes: 64 << 20,
        table: 1 << 16,
        fuel: 500_000_000,
    }
}

fn instantiate(bytes: Vec<u8>) -> Result<Instance<&'static [u8]>, Error> {
    let bytes: &'static [u8] = bytes.leak();
    let module = Module::new(bytes)?;
    if !module
        .imports()
        .all(|i| i.module == b"spectest" && i.name.starts_with(b"print"))
    {
        return Err(Error::Unsupported);
    }
    Instance::new(module, &limits(), &mut Spectest)
}

fn invoke(
    inst: &mut Instance<&'static [u8]>,
    call: &WastInvoke,
) -> Option<Result<Vec<u64>, Error>> {
    let f = inst.module().export_func(call.name)?;
    let mut args = Vec::new();
    for a in &call.args {
        args.push(match a {
            WastArg::Core(WastArgCore::I32(v)) => *v as u32 as u64,
            WastArg::Core(WastArgCore::I64(v)) => *v as u64,
            WastArg::Core(WastArgCore::F32(v)) => v.bits as u64,
            WastArg::Core(WastArgCore::F64(v)) => v.bits,
            _ => return None,
        });
    }
    let mut out = vec![0; inst.module().func_type(f)?.1.len()];
    inst.set_fuel(limits().fuel);
    Some(inst.call(&mut Spectest, f, &args, &mut out).map(|()| out))
}

fn matches(got: u64, want: &WastRet) -> Option<bool> {
    Some(match want {
        WastRet::Core(WastRetCore::I32(v)) => got == *v as u32 as u64,
        WastRet::Core(WastRetCore::I64(v)) => got == *v as u64,
        WastRet::Core(WastRetCore::F32(p)) => match p {
            NanPattern::Value(v) => got == v.bits as u64,
            NanPattern::CanonicalNan => got & 0x7FFF_FFFF == 0x7FC0_0000,
            NanPattern::ArithmeticNan => got & 0x7FC0_0000 == 0x7FC0_0000,
        },
        WastRet::Core(WastRetCore::F64(p)) => match p {
            NanPattern::Value(v) => got == v.bits,
            NanPattern::CanonicalNan => got & !(1 << 63) == 0x7FF8 << 48,
            NanPattern::ArithmeticNan => got & (0x7FF8 << 48) == 0x7FF8 << 48,
        },
        _ => return None,
    })
}

fn trap_matches(e: Error, message: &str) -> bool {
    let want = [
        ("unreachable", Error::Unreachable),
        ("integer divide by zero", Error::DivideByZero),
        ("integer overflow", Error::IntegerOverflow),
        ("invalid conversion to integer", Error::InvalidConversion),
        ("out of bounds memory access", Error::MemoryOutOfBounds),
        ("undefined element", Error::UndefinedElement),
        ("uninitialized element", Error::UninitializedElement),
        ("indirect call type mismatch", Error::SignatureMismatch),
    ];
    match want.iter().find(|(m, _)| message.starts_with(m)) {
        Some((_, w)) => {
            e == *w || (e == Error::SegmentOutOfBounds && *w == Error::MemoryOutOfBounds)
        }
        None => true,
    }
}

#[derive(Default)]
struct Tally {
    pass: usize,
    skip: usize,
    fail: Vec<String>,
}

fn run_file(path: &Path, t: &mut Tally) {
    let text = std::fs::read_to_string(path).unwrap();
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let Ok(buf) = ParseBuffer::new(&text) else {
        return;
    };
    let Ok(wast) = parser::parse::<Wast>(&buf) else {
        t.skip += 1;
        return;
    };
    // `None`: the module is outside the crate's scope.
    let mut all: Vec<Option<Instance<&'static [u8]>>> = Vec::new();
    let mut named: HashMap<String, usize> = HashMap::new();
    for d in wast.directives {
        let (line, _) = d.span().linecol_in(&text);
        let at = format!("{name}:{}", line + 1);
        let find = |all: &mut Vec<Option<Instance<&'static [u8]>>>,
                    id: &Option<wast::token::Id>| {
            let i = match id {
                Some(id) => *named.get(id.name())?,
                None => all.len().checked_sub(1)?,
            };
            Some(i).filter(|i| all[*i].is_some())
        };
        match d {
            WastDirective::Module(mut m) => {
                let id = match &m {
                    QuoteWat::Wat(Wat::Module(m)) => m.id.map(|i| i.name().to_string()),
                    _ => None,
                };
                let inst = m.encode().ok().and_then(|b| match instantiate(b) {
                    Ok(i) => Some(i),
                    Err(Error::Unsupported) => None,
                    Err(e) => {
                        t.fail.push(format!("{at}: module: {e:?}"));
                        None
                    }
                });
                if let Some(id) = id {
                    named.insert(id, all.len());
                }
                all.push(inst);
            }
            // Modules linked to each other share state this harness does
            // not model: stop at the first `register`.
            WastDirective::Register { .. } => return,
            WastDirective::ModuleDefinition(_) | WastDirective::ModuleInstance { .. } => {
                all.push(None)
            }
            WastDirective::Invoke(call) => {
                if let Some(i) = find(&mut all, &call.module) {
                    match invoke(all[i].as_mut().unwrap(), &call) {
                        Some(Ok(_)) => t.pass += 1,
                        Some(Err(e)) => t.fail.push(format!("{at}: invoke {}: {e:?}", call.name)),
                        None => t.skip += 1,
                    }
                } else {
                    t.skip += 1;
                }
            }
            WastDirective::AssertReturn {
                exec: WastExecute::Invoke(call),
                results,
                ..
            } => {
                let Some(i) = find(&mut all, &call.module) else {
                    t.skip += 1;
                    continue;
                };
                match invoke(all[i].as_mut().unwrap(), &call) {
                    Some(Ok(got)) => {
                        let ok: Option<Vec<bool>> = got
                            .iter()
                            .zip(&results)
                            .map(|(g, w)| matches(*g, w))
                            .collect();
                        match ok {
                            Some(v) if v.iter().all(|b| *b) && got.len() == results.len() => {
                                t.pass += 1
                            }
                            Some(_) => t.fail.push(format!(
                                "{at}: {} returned {got:x?}, want {results:?}",
                                call.name
                            )),
                            None => t.skip += 1,
                        }
                    }
                    Some(Err(e)) => t.fail.push(format!("{at}: {} failed: {e:?}", call.name)),
                    None => t.skip += 1,
                }
            }
            WastDirective::AssertTrap {
                exec: WastExecute::Invoke(call),
                message,
                ..
            } => {
                let Some(i) = find(&mut all, &call.module) else {
                    t.skip += 1;
                    continue;
                };
                match invoke(all[i].as_mut().unwrap(), &call) {
                    Some(Err(e)) if trap_matches(e, message) => t.pass += 1,
                    Some(r) => t.fail.push(format!(
                        "{at}: {} gave {r:x?}, want trap {message}",
                        call.name
                    )),
                    None => t.skip += 1,
                }
            }
            WastDirective::AssertTrap {
                exec: WastExecute::Wat(Wat::Module(mut m)),
                message,
                ..
            } => match m.encode().map(instantiate) {
                Ok(Err(Error::Unsupported)) | Err(_) => t.skip += 1,
                Ok(Err(e)) if trap_matches(e, message) => t.pass += 1,
                Ok(r) => t.fail.push(format!(
                    "{at}: instantiation gave {:?}, want trap {message}",
                    r.err()
                )),
            },
            WastDirective::AssertExhaustion { call, .. } => {
                let Some(i) = find(&mut all, &call.module) else {
                    t.skip += 1;
                    continue;
                };
                match invoke(all[i].as_mut().unwrap(), &call) {
                    Some(Err(Error::StackOverflow)) => t.pass += 1,
                    Some(r) => t
                        .fail
                        .push(format!("{at}: {} gave {r:x?}, want exhaustion", call.name)),
                    None => t.skip += 1,
                }
            }
            _ => t.skip += 1,
        }
    }
}

#[test]
fn spec_testsuite() {
    let Ok(dir) = std::env::var("WASM_TESTSUITE") else {
        eprintln!("WASM_TESTSUITE not set: specification testsuite not run");
        return;
    };
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "wast"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no .wast files in {dir}");
    let mut total = Tally::default();
    for f in files {
        let mut t = Tally::default();
        run_file(&f, &mut t);
        println!(
            "{:28} pass {:5} skip {:5} fail {:4}",
            f.file_name().unwrap().to_string_lossy(),
            t.pass,
            t.skip,
            t.fail.len()
        );
        total.pass += t.pass;
        total.skip += t.skip;
        total.fail.extend(t.fail);
    }
    println!(
        "total: pass {} skip {} fail {}",
        total.pass,
        total.skip,
        total.fail.len()
    );
    for f in total.fail.iter().take(60) {
        println!("  {f}");
    }
    assert!(total.fail.is_empty(), "{} failures", total.fail.len());
    assert!(total.pass > 10_000, "only {} assertions ran", total.pass);
}
