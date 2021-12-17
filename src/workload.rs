use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use hdrhistogram::Histogram;
use rune::runtime::{AnyObj, Args, RuntimeContext, Shared, VmError};
use rune::termcolor::{ColorChoice, StandardStream};
use rune::{Any, Diagnostics, Module, Source, Sources, ToValue, Unit, Value, Vm};
use try_lock::TryLock;

use crate::error::LatteError;
use crate::{CassError, CassErrorKind, Context, SessionStats};

/// Wraps a reference to Session that can be converted to a Rune `Value`
/// and passed as one of `Args` arguments to a function.
struct SessionRef<'a> {
    context: &'a Context,
}

impl SessionRef<'_> {
    pub fn new(context: &Context) -> SessionRef {
        SessionRef { context }
    }
}

/// We need this to be able to pass a reference to `Session` as an argument
/// to Rune function.
///
/// Caution! Be careful using this trait. Undefined Behaviour possible.
/// This is unsound - it is theoretically
/// possible that the underlying `Session` gets dropped before the `Value` produced by this trait
/// implementation and the compiler is not going to catch that.
/// The receiver of a `Value` must ensure that it is dropped before `Session`!
impl<'a> ToValue for SessionRef<'a> {
    fn to_value(self) -> Result<Value, VmError> {
        let obj = unsafe { AnyObj::from_ref(self.context) };
        Ok(Value::from(Shared::new(obj)))
    }
}

/// Wraps a mutable reference to Session that can be converted to a Rune `Value` and passed
/// as one of `Args` arguments to a function.
struct ContextRefMut<'a> {
    context: &'a mut Context,
}

impl ContextRefMut<'_> {
    pub fn new(context: &mut Context) -> ContextRefMut {
        ContextRefMut { context }
    }
}

/// Caution! See `impl ToValue for SessionRef`.
impl<'a> ToValue for ContextRefMut<'a> {
    fn to_value(self) -> Result<Value, VmError> {
        let obj = unsafe { AnyObj::from_mut(self.context) };
        Ok(Value::from(Shared::new(obj)))
    }
}

/// Stores the name and hash together.
/// Name is used for message formatting, hash is used for fast function lookup.
#[derive(Debug, Copy, Clone)]
pub struct FnRef {
    name: &'static str,
    hash: rune::Hash,
}

impl FnRef {
    pub fn new(name: &'static str) -> FnRef {
        FnRef {
            name,
            hash: rune::Hash::type_hash(&[name]),
        }
    }
}

pub const SCHEMA_FN: &str = "schema";
pub const PREPARE_FN: &str = "prepare";
pub const ERASE_FN: &str = "erase";
pub const LOAD_FN: &str = "load";
pub const RUN_FN: &str = "run";

/// Compiled workload program
#[derive(Clone)]
pub struct Program {
    sources: Arc<Sources>,
    context: Arc<RuntimeContext>,
    unit: Arc<Unit>,
}

impl Program {
    /// Performs some basic sanity checks of the workload script source and prepares it
    /// for fast execution. Does not create VM yet.
    ///
    /// # Parameters
    /// - `script`: source code in Rune language
    /// - `params`: parameter values that will be exposed to the script by the `params!` macro
    pub fn new(source: Source, params: HashMap<String, String>) -> Result<Program, LatteError> {
        let mut context_module = Module::default();
        context_module.ty::<Context>().unwrap();
        context_module
            .async_inst_fn("execute", Context::execute)
            .unwrap();
        context_module
            .async_inst_fn("prepare", Context::prepare)
            .unwrap();
        context_module
            .async_inst_fn("execute_prepared", Context::execute_prepared)
            .unwrap();

        let mut err_module = Module::default();
        err_module.ty::<CassError>().unwrap();
        err_module
            .inst_fn(rune::runtime::Protocol::STRING_DISPLAY, CassError::display)
            .unwrap();

        let mut uuid_module = Module::default();
        uuid_module.ty::<globals::Uuid>().unwrap();
        uuid_module
            .inst_fn(
                rune::runtime::Protocol::STRING_DISPLAY,
                globals::Uuid::display,
            )
            .unwrap();

        let mut latte_module = Module::with_crate("latte");
        latte_module.function(&["i32"], globals::to_i32).unwrap();
        latte_module.function(&["i16"], globals::to_i16).unwrap();
        latte_module.function(&["i8"], globals::to_i8).unwrap();
        latte_module.function(&["blob"], globals::blob).unwrap();
        latte_module.function(&["hash"], globals::hash).unwrap();
        latte_module.function(&["hash2"], globals::hash2).unwrap();
        latte_module
            .function(&["hash_range"], globals::hash_range)
            .unwrap();
        latte_module
            .function(&["uuid"], globals::Uuid::new)
            .unwrap();
        latte_module
            .macro_(&["param"], move |ctx, ts| globals::param(ctx, &params, ts))
            .unwrap();

        let mut context = rune::Context::with_default_modules().unwrap();
        context.install(&context_module).unwrap();
        context.install(&err_module).unwrap();
        context.install(&uuid_module).unwrap();
        context.install(&latte_module).unwrap();

        let mut options = rune::Options::default();
        options.debug_info(true);

        let mut diagnostics = Diagnostics::new();
        let mut sources = Sources::new();
        sources.insert(source);

        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .with_diagnostics(&mut diagnostics)
            .build();

        if !diagnostics.is_empty() {
            let mut writer = StandardStream::stderr(ColorChoice::Always);
            diagnostics.emit(&mut writer, &sources)?;
        }
        let unit = unit?;

        Ok(Program {
            sources: Arc::new(sources),
            context: Arc::new(context.runtime()),
            unit: Arc::new(unit),
        })
    }

    /// Makes a deep copy of context and unit.
    /// Calling this method instead of `clone` ensures that Rune runtime structures
    /// are separate and can be moved to different CPU cores efficiently without accidental
    /// sharing of Arc references.
    fn unshare(&self) -> Program {
        Program {
            sources: self.sources.clone(),
            context: Arc::new(self.context.as_ref().clone()),
            unit: Arc::new(self.unit.as_ref().clone()),
        }
    }

    /// Initializes a fresh virtual machine needed to execute this program.
    /// This is extremely lightweight.
    fn vm(&self) -> Vm {
        Vm::new(self.context.clone(), self.unit.clone())
    }

    /// Checks if Rune function call result is an error and if so, converts it into [`LatteError`].
    /// Cassandra errors are returned as [`LatteError::Cassandra`].
    /// All other errors are returned as [`LatteError::FunctionResult`].
    /// If result is not an `Err`, it is returned as-is.
    ///
    /// This is needed because execution of the function could actually run till completion just
    /// fine, but the function could return an error value, and in this case we should not
    /// ignore it.
    fn convert_error(
        &self,
        function_name: &'static str,
        result: Value,
    ) -> Result<Value, LatteError> {
        match result {
            Value::Result(result) => match result.take().unwrap() {
                Ok(value) => Ok(value),
                Err(Value::Any(e)) => {
                    if e.borrow_ref().unwrap().type_hash() == CassError::type_hash() {
                        let e = e.take_downcast::<CassError>().unwrap();
                        return Err(LatteError::Cassandra(e));
                    }
                    let mut msg = String::new();
                    let mut buf = String::new();
                    let e = Value::Any(e);
                    self.vm().with(|| {
                        if e.string_display(&mut msg, &mut buf).unwrap().is_err() {
                            msg = format!("{:?}", e)
                        }
                    });
                    Err(LatteError::FunctionResult(function_name, msg))
                }
                Err(other) => Err(LatteError::FunctionResult(
                    function_name,
                    format!("{:?}", other),
                )),
            },
            other => Ok(other),
        }
    }

    /// Executes given async function with args.
    /// If execution fails, emits diagnostic messages, e.g. stacktrace to standard error stream.
    /// Also signals an error if the function execution succeeds, but the function returns
    /// an error value.
    pub async fn async_call(
        &self,
        fun: FnRef,
        args: impl Args + Send,
    ) -> Result<Value, LatteError> {
        let handle_err = |e: VmError| {
            let mut out = StandardStream::stderr(ColorChoice::Auto);
            let _ = e.emit(&mut out, &self.sources);
            LatteError::ScriptExecError(fun.name, e)
        };
        let execution = self.vm().send_execute(fun.hash, args).map_err(handle_err)?;
        let result = execution.async_complete().await.map_err(handle_err)?;
        self.convert_error(fun.name, result)
    }

    pub fn has_prepare(&self) -> bool {
        self.unit.function(FnRef::new(PREPARE_FN).hash).is_some()
    }

    pub fn has_schema(&self) -> bool {
        self.unit.function(FnRef::new(SCHEMA_FN).hash).is_some()
    }

    pub fn has_erase(&self) -> bool {
        self.unit.function(FnRef::new(ERASE_FN).hash).is_some()
    }

    pub fn has_load(&self) -> bool {
        self.unit.function(FnRef::new(LOAD_FN).hash).is_some()
    }

    pub fn has_run(&self) -> bool {
        self.unit.function(FnRef::new(RUN_FN).hash).is_some()
    }

    /// Calls the script's `init` function.
    /// Called once at the beginning of the benchmark.
    /// Typically used to prepare statements.
    pub async fn prepare(&mut self, context: &mut Context) -> Result<(), LatteError> {
        let context = ContextRefMut::new(context);
        self.async_call(FnRef::new(PREPARE_FN), (context,)).await?;
        Ok(())
    }

    /// Calls the script's `schema` function.
    /// Typically used to create database schema.
    pub async fn schema(&mut self, context: &mut Context) -> Result<(), LatteError> {
        let context = ContextRefMut::new(context);
        self.async_call(FnRef::new(SCHEMA_FN), (context,)).await?;
        Ok(())
    }

    /// Calls the script's `erase` function.
    /// Typically used to remove the data from the database before running the benchmark.
    pub async fn erase(&mut self, context: &mut Context) -> Result<(), LatteError> {
        let context = ContextRefMut::new(context);
        self.async_call(FnRef::new(ERASE_FN), (context,)).await?;
        Ok(())
    }
}

/// Tracks statistics of the Rune function invoked by the workload
#[derive(Clone, Debug)]
pub struct FnStats {
    pub call_count: u64,
    pub call_times_ns: Histogram<u64>,
}

impl FnStats {
    pub fn operation_completed(&mut self, duration: Duration) {
        self.call_count += 1;
        self.call_times_ns
            .record(duration.as_nanos().clamp(1, u64::MAX as u128) as u64)
            .unwrap();
    }
}

impl Default for FnStats {
    fn default() -> Self {
        FnStats {
            call_count: 0,
            call_times_ns: Histogram::new(3).unwrap(),
        }
    }
}

/// Statistics of operations (function calls) and Cassandra requests.
pub struct WorkloadStats {
    pub start_time: Instant,
    pub end_time: Instant,
    pub function_stats: FnStats,
    pub session_stats: SessionStats,
}

/// Mutable part of Workload
pub struct WorkloadState {
    start_time: Instant,
    fn_stats: FnStats,
}

impl Default for WorkloadState {
    fn default() -> Self {
        WorkloadState {
            start_time: Instant::now(),
            fn_stats: Default::default(),
        }
    }
}

pub struct Workload {
    context: Context,
    program: Program,
    function: FnRef,
    state: TryLock<WorkloadState>,
}

impl Clone for Workload {
    fn clone(&self) -> Self {
        Workload {
            context: self.context.clone(),
            // make a deep copy to avoid congestion on Arc ref counts used heavily by Rune
            program: self.program.unshare(),
            function: self.function,
            state: TryLock::new(WorkloadState::default()),
        }
    }
}

impl Workload {
    pub fn new(context: Context, program: Program, function: FnRef) -> Workload {
        Workload {
            context,
            program,
            function,
            state: TryLock::new(WorkloadState::default()),
        }
    }

    /// Executes a single cycle of a workload.
    /// This should be idempotent –
    /// the generated action should be a function of the iteration number.
    /// Returns the cycle number and the end time of the query.
    pub async fn run(&self, cycle: u64) -> Result<(u64, Instant), LatteError> {
        let start_time = Instant::now();
        let context = SessionRef::new(&self.context);
        let result = self
            .program
            .async_call(self.function, (context, cycle as i64))
            .await
            .map(|_| ()); // erase Value, because Value is !Send
        let end_time = Instant::now();
        let mut state = self.state.try_lock().unwrap();
        state.fn_stats.operation_completed(end_time - start_time);
        match result {
            Ok(_) => Ok((cycle, end_time)),
            Err(LatteError::Cassandra(CassError(CassErrorKind::Overloaded(_)))) => {
                // don't stop on overload errors;
                // they are being counted by the context stats anyways
                Ok((cycle, end_time))
            }
            Err(e) => Err(e),
        }
    }

    /// Returns the reference to the contained context.
    /// Allows to e.g. access context stats.
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// Sets the workload start time and resets the counters.
    /// Needed for producing `WorkloadStats` with
    /// recorded start and end times of measurement.
    pub fn reset(&self, start_time: Instant) {
        let mut state = self.state.try_lock().unwrap();
        state.fn_stats = FnStats::default();
        state.start_time = start_time;
        self.context.reset_session_stats();
    }

    /// Returns statistics of the operations invoked by this workload so far.
    /// Resets the internal statistic counters.
    pub fn take_stats(&self, end_time: Instant) -> WorkloadStats {
        let mut state = self.state.try_lock().unwrap();
        let result = WorkloadStats {
            start_time: state.start_time,
            end_time,
            function_stats: state.fn_stats.clone(),
            session_stats: self.context().take_session_stats(),
        };
        state.start_time = end_time;
        state.fn_stats = FnStats::default();
        result
    }
}

pub mod globals {
    use std::collections::HashMap;
    use std::hash::{Hash, Hasher};

    use anyhow::anyhow;
    use itertools::Itertools;
    use metrohash::{MetroHash128, MetroHash64};
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use rune::ast::Kind;
    use rune::macros::{quote, MacroContext, TokenStream};
    use rune::parse::Parser;
    use rune::{ast, Any};
    use uuid::{Variant, Version};

    #[derive(Clone, Debug, Any)]
    pub struct Uuid(pub uuid::Uuid);

    impl Uuid {
        pub fn new(i: i64) -> Uuid {
            let mut hash = MetroHash128::new();
            i.hash(&mut hash);
            let (h1, h2) = hash.finish128();
            let h = ((h1 as u128) << 64) | (h2 as u128);
            Uuid(
                uuid::Builder::from_u128(h)
                    .set_variant(Variant::RFC4122)
                    .set_version(Version::Random)
                    .build(),
            )
        }

        pub fn display(&self, buf: &mut String) -> std::fmt::Result {
            use std::fmt::Write;
            write!(buf, "{}", self.0)
        }
    }

    #[derive(Clone, Debug, Any)]
    pub struct Int8(pub i8);

    #[derive(Clone, Debug, Any)]
    pub struct Int16(pub i16);

    #[derive(Clone, Debug, Any)]
    pub struct Int32(pub i32);


    /// Returns the literal value stored in the `params` map under the key given as the first
    /// macro arg, and if not found, returns the expression from the second arg.
    pub fn param(
        ctx: &mut MacroContext,
        params: &HashMap<String, String>,
        ts: &TokenStream,
    ) -> rune::Result<TokenStream> {
        let mut parser = Parser::from_token_stream(ts, ctx.macro_span());
        let name = parser.parse::<ast::LitStr>()?;
        let name = ctx.resolve(name)?.to_string();
        let sep = parser.next()?;
        if sep.kind != Kind::Comma {
            return Err(anyhow!("Expected comma"));
        }
        let expr = parser.parse::<ast::Expr>()?;
        let rhs = match params.get(&name) {
            Some(value) => {
                let src_id = ctx.insert_source(&name, value);
                let value = ctx.parse_source::<ast::Expr>(src_id)?;
                quote!(#value)
            }
            None => quote!(#expr),
        };
        Ok(rhs.into_token_stream(ctx))
    }


    /// Converts a Rune integer to i8 (Cassandra tinyint)
    pub fn to_i8(value: i64) -> Option<Int8> {
        Some(Int8(value.try_into().ok()?))
    }

    /// Converts a Rune integer to i16 (Cassandra smallint)
    pub fn to_i16(value: i64) -> Option<Int16> {
        Some(Int16(value.try_into().ok()?))
    }

    /// Converts a Rune integer to i32 (Cassandra int)
    pub fn to_i32(value: i64) -> Option<Int32> {
        Some(Int32(value.try_into().ok()?))
    }

    /// Computes a hash of an integer value `i`.
    /// Returns a value in range `0..i64::MAX`.
    pub fn hash(i: i64) -> i64 {
        let mut hash = MetroHash64::new();
        i.hash(&mut hash);
        (hash.finish() & 0x7FFFFFFFFFFFFFFF) as i64
    }

    /// Computes hash of two integer values.
    pub fn hash2(a: i64, b: i64) -> i64 {
        let mut hash = MetroHash64::new();
        a.hash(&mut hash);
        b.hash(&mut hash);
        (hash.finish() & 0x7FFFFFFFFFFFFFFF) as i64
    }

    /// Computes a hash of an integer value `i`.
    /// Returns a value in range `0..max`.
    pub fn hash_range(i: i64, max: i64) -> i64 {
        hash(i) % max
    }

    /// Generates random blob of data of given length.
    /// Parameter `seed` is used to seed the RNG.
    pub fn blob(seed: i64, len: usize) -> rune::runtime::Bytes {
        let mut rng = StdRng::seed_from_u64(seed as u64);
        let v = (0..len).map(|_| rng.gen()).collect_vec();
        rune::runtime::Bytes::from_vec(v)
    }
}
