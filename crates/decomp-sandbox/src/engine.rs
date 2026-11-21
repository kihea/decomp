//! The WebAssembly engine the sandbox runs on, chosen when the crate is
//! built: wasmtime, a compiler, on computers; or wasmi, an interpreter, where
//! code cannot be compiled at run time, as in a canister.
//!
//! Both run the same metered module (see `meter`), so a job's compute bill
//! and checkpoints do not depend on the engine. The host functions are
//! written once, against the names re-exported here.

#[cfg(all(feature = "wasmtime", feature = "wasmi"))]
compile_error!("choose one engine: the `wasmtime` feature or the `wasmi` feature");

#[cfg(not(any(feature = "wasmtime", feature = "wasmi")))]
compile_error!("choose an engine: the `wasmtime` feature or the `wasmi` feature");

#[cfg(feature = "wasmtime")]
mod chosen {
    pub use wasmtime::{
        Caller, Engine, Extern, ExternType, Linker, Module, Store, StoreLimits, StoreLimitsBuilder,
        Val,
    };

    /// What a host function returns.
    pub type HostResult<T> = wasmtime::Result<T>;
    pub type EngineError = wasmtime::Error;

    pub fn new_engine() -> Engine {
        let mut config = wasmtime::Config::new();
        config
            .cranelift_nan_canonicalization(true)
            .relaxed_simd_deterministic(true)
            // Features the meter does not account for are switched off, so a
            // module that uses them is rejected instead of being metered wrongly.
            .wasm_tail_call(false)
            .wasm_memory64(false)
            // Backtraces vary with how a module was compiled and are not part of a result.
            .wasm_backtrace_max_frames(None);
        // Threads and exceptions need no setting: this crate builds wasmtime
        // without the features behind them, so modules that use either are
        // rejected.
        Engine::new(&config).expect("the sandbox configuration is valid")
    }

    /// Instantiates a module, resolving its imports, and runs its start
    /// function if it has one.
    pub fn instantiate<T: 'static>(
        linker: &Linker<T>,
        store: &mut Store<T>,
        module: &Module,
    ) -> Result<wasmtime::Instance, EngineError> {
        linker.instantiate(store, module)
    }

    /// Resolves a module's imports without running anything, so that from then
    /// on only the job itself can fail.
    pub fn check_imports<T: 'static>(
        linker: &Linker<T>,
        module: &Module,
        _host_functions: &[(&str, &str)],
    ) -> Result<(), EngineError> {
        linker.instantiate_pre(module).map(|_| ())
    }

    /// The reason a run stopped, as a trap message.
    pub fn trap_message(error: &EngineError) -> String {
        match error.downcast_ref::<wasmtime::Trap>() {
            Some(trap) => trap.to_string(),
            // A host function refused a call, or the engine enforced a limit.
            None => error.root_cause().to_string(),
        }
    }

    pub fn host_error<E: std::error::Error + Send + Sync + 'static>(
        error: &EngineError,
    ) -> Option<&E> {
        error.downcast_ref::<E>()
    }
}

#[cfg(feature = "wasmi")]
mod chosen {
    pub use wasmi::{
        Caller, Engine, Extern, ExternType, Linker, Module, Store, StoreLimits, StoreLimitsBuilder,
        Val,
    };

    /// What a host function returns.
    pub type HostResult<T> = Result<T, wasmi::Error>;
    pub type EngineError = wasmi::Error;

    pub fn new_engine() -> Engine {
        let mut config = wasmi::Config::default();
        // Features the meter does not account for are switched off, so a
        // module that uses them is rejected instead of being metered wrongly.
        // 64-bit memories are not built in at all.
        config.wasm_tail_call(false);
        Engine::new(&config)
    }

    /// Instantiates a module, resolving its imports, and runs its start
    /// function if it has one.
    pub fn instantiate<T>(
        linker: &Linker<T>,
        store: &mut Store<T>,
        module: &Module,
    ) -> Result<wasmi::Instance, EngineError> {
        linker.instantiate_and_start(store, module)
    }

    /// Checks, without running anything, that a module imports nothing but
    /// the host functions, so that from then on only the job itself can fail.
    pub fn check_imports<T>(
        _linker: &Linker<T>,
        module: &Module,
        host_functions: &[(&str, &str)],
    ) -> Result<(), EngineError> {
        for import in module.imports() {
            let known = host_functions
                .iter()
                .any(|&(module, name)| import.module() == module && import.name() == name);
            if !known {
                let message = format!(
                    "{}.{} is not a host function",
                    import.module(),
                    import.name()
                );
                return Err(wasmi::Error::new(message));
            }
        }
        Ok(())
    }

    /// The reason a run stopped, as a trap message.
    pub fn trap_message(error: &EngineError) -> String {
        error.to_string()
    }

    pub fn host_error<E: wasmi::errors::HostError>(error: &EngineError) -> Option<&E> {
        error.downcast_ref::<E>()
    }

    impl wasmi::errors::HostError for crate::Halt {}

    impl From<crate::Halt> for wasmi::Error {
        fn from(halt: crate::Halt) -> Self {
            wasmi::Error::host(halt)
        }
    }
}

pub use chosen::*;
