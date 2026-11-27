//! Loads modules and calls an export with a host that answers 0 to every
//! import: `cargo run -p wasm-micro --example probe -- init a.wasm b.wasm`.
use wasm_micro::{Error, Host, Instance, Limits, Module};

struct Zero(u32);
impl Host for Zero {
    fn call(&mut self, _: u32, _: &mut [u8], vals: &mut [u64]) -> Result<(), Error> {
        self.0 += 1;
        vals.fill(0);
        Ok(())
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let export = args.next().expect("export name");
    for path in args {
        let bytes = std::fs::read(&path).unwrap();
        let name = path.rsplit('/').next().unwrap();
        let module = match Module::new(&bytes[..]) {
            Ok(m) => m,
            Err(e) => {
                println!("{name:24} {:7} bytes: load {e:?}", bytes.len());
                continue;
            }
        };
        let f = module.export_func(&export);
        let imports = module.import_count();
        let mut host = Zero(0);
        let limits = Limits {
            fuel: 1_000_000,
            ..Limits::default()
        };
        let r = Instance::new(module, &limits, &mut host).and_then(|mut i| {
            let f = f.ok_or(Error::Invalid)?;
            i.call(&mut host, f, &[], &mut [])?;
            Ok(1_000_000 - i.fuel())
        });
        println!(
            "{name:24} {:7} bytes, {imports:2} imports: {export} -> {r:?} ({} host calls)",
            bytes.len(),
            host.0
        );
    }
}
