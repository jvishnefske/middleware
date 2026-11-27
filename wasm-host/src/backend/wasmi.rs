//! The wasmi backend (feature `wasmi`, the default): modules are validated
//! and translated to wasmi bytecode at load.

use alloc::string::ToString;

use middleware::wasm_abi;
use wasmi::{CompilationMode, Config, Engine, ExternType, FuncType, Store, TrapCode, ValType};

use crate::abi::{ImportKind, Shape};
use crate::link::{self, State};
use crate::{Device, Limits, LoadError, Trap};

pub(crate) const NAME: &str = "wasmi";

/// What an export is, as [`crate::validate`] needs it.
pub(crate) enum Export {
    /// A function; `None` when it uses a non-`i32` type.
    Func(Option<Shape>),
    Memory,
    Other,
}

/// The wasmi engine configuration for these limits: fuel metering, custom
/// page sizes, eager translation and the stack caps.
fn config(limits: &Limits) -> Config {
    let mut config = Config::default();
    let stack = limits.max_stack_bytes as usize;
    // Order matters: wasmi panics if the minimum ever exceeds the
    // maximum. Lowering the minimum first makes every step valid.
    config
        .consume_fuel(true)
        .wasm_custom_page_sizes(true)
        .compilation_mode(CompilationMode::Eager)
        .set_max_recursion_depth(limits.max_call_depth as usize)
        .set_min_stack_height(0)
        .set_max_stack_height(stack)
        .set_min_stack_height(stack)
        .set_max_cached_stacks(1);
    config
}

/// Converts a wasmi function type to an all-`i32` [`Shape`].
fn shape(ty: &FuncType) -> Option<Shape> {
    let all_i32 = |v: &[ValType]| v.iter().all(|t| *t == ValType::I32);
    (all_i32(ty.params()) && all_i32(ty.results())).then(|| Shape {
        params: ty.params().len(),
        results: ty.results().len(),
    })
}

/// Classifies a wasmi error as a [`Trap`].
fn trap(e: &wasmi::Error) -> Trap {
    match e.as_trap_code() {
        Some(TrapCode::OutOfFuel) => Trap::OutOfFuel,
        _ => Trap::Wasm(e.to_string()),
    }
}

/// A validated module, translated eagerly.
pub(crate) struct Module {
    engine: Engine,
    module: wasmi::Module,
}

impl Module {
    pub fn new(wasm: &[u8], limits: &Limits) -> Result<Self, LoadError> {
        let engine = Engine::new(&config(limits));
        let module =
            wasmi::Module::new(&engine, wasm).map_err(|e| LoadError::Invalid(e.to_string()))?;
        Ok(Module { engine, module })
    }

    /// Calls `f` with every import's module, name and kind.
    pub fn imports(
        &self,
        f: &mut dyn FnMut(&str, &str, ImportKind) -> Result<(), LoadError>,
    ) -> Result<(), LoadError> {
        for import in self.module.imports() {
            let kind = match import.ty() {
                ExternType::Func(ty) => ImportKind::Func(shape(ty)),
                _ => ImportKind::Other,
            };
            f(import.module(), import.name(), kind)?;
        }
        Ok(())
    }

    /// Calls `f` with every export's name and kind.
    pub fn exports(
        &self,
        f: &mut dyn FnMut(&str, Export) -> Result<(), LoadError>,
    ) -> Result<(), LoadError> {
        for export in self.module.exports() {
            let kind = match export.ty() {
                ExternType::Func(ty) => Export::Func(shape(ty)),
                ExternType::Memory(_) => Export::Memory,
                _ => Export::Other,
            };
            f(export.name(), kind)?;
        }
        Ok(())
    }
}

/// An instantiated module and the payload's [`State`], in a wasmi store.
///
/// `D: 'static` is required because wasmi stores host closures and the
/// store's resource limiter as `'static` values parameterised by `D`.
pub(crate) struct Instance<D: Device + 'static> {
    store: Store<State<D>>,
    instance: wasmi::Instance,
}

impl<D: Device + 'static> Instance<D> {
    /// Links the imports, instantiates and runs the start function.
    pub fn new(module: Module, state: State<D>, limits: &Limits) -> Result<Self, LoadError> {
        let Module { engine, module } = module;
        let mut store = Store::new(&engine, state);
        store.limiter(|s| &mut s.limits);
        let instantiate = |e: wasmi::Error| LoadError::Instantiate(e.to_string());
        let linker = link::linker::<D>(&engine).map_err(instantiate)?;
        store.set_fuel(limits.fuel_per_call).map_err(instantiate)?;
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(instantiate)?;
        Ok(Instance { store, instance })
    }

    fn typed<P, R>(&mut self, name: &str, params: P) -> Result<R, wasmi::Error>
    where
        P: wasmi::WasmParams,
        R: wasmi::WasmResults,
    {
        self.instance
            .get_typed_func::<P, R>(&self.store, name)
            .and_then(|f| f.call(&mut self.store, params))
    }

    /// Calls the export `name` on `fuel`. The shapes are those of the
    /// ABI's exports; any other is a trap.
    pub fn call(
        &mut self,
        name: &str,
        args: &[i32],
        returns: bool,
        fuel: u64,
    ) -> Result<i32, Trap> {
        let result = self
            .store
            .set_fuel(fuel)
            .and_then(|()| match (args, returns) {
                ([], false) => self.typed::<(), ()>(name, ()).map(|()| 0),
                (&[a], false) => self.typed::<i32, ()>(name, a).map(|()| 0),
                (&[a], true) => self.typed::<i32, i32>(name, a),
                (&[a, b, c], false) => self
                    .typed::<(i32, i32, i32), ()>(name, (a, b, c))
                    .map(|()| 0),
                (&[a, b, c, d], false) => self
                    .typed::<(i32, i32, i32, i32), ()>(name, (a, b, c, d))
                    .map(|()| 0),
                (&[a, b, c, d, e, g], true) => {
                    self.typed::<(i32, i32, i32, i32, i32, i32), i32>(name, (a, b, c, d, e, g))
                }
                _ => Err(wasmi::Error::new("export shape outside the ABI")),
            });
        result.map_err(|e| trap(&e))
    }

    /// Fuel left after the last call.
    pub fn fuel_left(&self) -> Option<u64> {
        self.store.get_fuel().ok()
    }

    pub fn state(&self) -> &State<D> {
        self.store.data()
    }

    pub fn state_mut(&mut self) -> &mut State<D> {
        self.store.data_mut()
    }

    pub fn into_state(self) -> State<D> {
        self.store.into_data()
    }

    /// The exported linear memory; empty when there is none.
    pub fn memory(&self) -> &[u8] {
        self.instance
            .get_memory(&self.store, wasm_abi::exports::MEMORY)
            .map(|m| m.data(&self.store))
            .unwrap_or_default()
    }

    pub fn memory_mut(&mut self) -> &mut [u8] {
        match self
            .instance
            .get_memory(&self.store, wasm_abi::exports::MEMORY)
        {
            Some(m) => m.data_mut(&mut self.store),
            None => &mut [],
        }
    }
}
