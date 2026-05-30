use crate::{ResultCode, Value};
use std::{
    ffi::{c_char, c_void},
    fmt::Display,
};

pub type ContextDestructor = unsafe extern "C" fn(context: usize);
pub type ValueDestructor = unsafe extern "C" fn(result: *mut Value);
pub type ScalarFunction = unsafe extern "C" fn(
    context: usize,
    argc: i32,
    argv: *const Value,
    context_destructor: Option<ContextDestructor>,
    value_destructor: Option<ValueDestructor>,
) -> Value;

/// `sqlite3_rtree_geometry_callback`-style geometry function signature.
/// Called per-cell during rtree MATCH queries.
///
/// Parameters:
/// - `n_dim`: number of dimensions (2 for 2D, 4 for 3D, etc.)
/// - `coords`: pointer to cell bounding-box coordinates [xmin, xmax, ymin, ymax, ...]
/// - `n_param`: number of user parameters (from the SQL function args)
/// - `params`: pointer to user parameter values
/// - `user_context`: opaque user-provided context from registration
/// - `result`: output — the callback writes 1 to accept the cell, 0 to reject
///
/// Returns: 0 on error (constraint violated), non-zero to continue evaluation
pub type GeometryCallbackFn = unsafe extern "C" fn(
    n_dim: i32,
    coords: *const f32,
    n_param: i32,
    params: *const f64,
    user_context: usize,
    result: *mut i32,
) -> i32;

/// Argument passed as `conn_ctx` to [`RegisterScalarFnWithCtx`] callbacks.
/// This is an opaque pointer to the extension's `Conn` wrapper.
/// Extensions cast this to `*const turso_ext::Conn` and wrap it in
/// `Arc::new(Connection::new(ptr))` to get the full connection API.
pub type ScalarFunctionConnCtx = *mut std::ffi::c_void;

pub type RegisterScalarFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    name: *const c_char,
    argc: i32,
    deterministic: bool,
    context: usize,
    func: ScalarFunction,
    context_destructor: Option<ContextDestructor>,
    value_destructor: Option<ValueDestructor>,
) -> ResultCode;

/// Register a scalar function that receives a connection context as the second argument.
/// The function signature is: `fn(context, conn: ScalarFunctionConnCtx, argc, argv, ...)`.
///
/// This is required for geometry callbacks like `sqlite3_rtree_geometry_callback` which
/// need the connection to register auxiliary SQL state. The `conn` argument is an opaque
/// pointer to the connection's extension handle; extensions should treat it as `*const Conn`
/// and wrap it in `Arc::new(Connection::new(conn))` to access the prepared-statement API.
pub type RegisterScalarFnWithCtx = unsafe extern "C" fn(
    ctx: *mut c_void,
    name: *const c_char,
    argc: i32,
    deterministic: bool,
    context: usize,
    func: unsafe extern "C" fn(
        context: usize,
        conn: ScalarFunctionConnCtx,
        argc: i32,
        argv: *const Value,
        context_destructor: Option<ContextDestructor>,
        value_destructor: Option<ValueDestructor>,
    ) -> Value,
    context_destructor: Option<ContextDestructor>,
    value_destructor: Option<ValueDestructor>,
) -> ResultCode;

pub type UnregisterFunctionFn =
    unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char) -> ResultCode;

pub type RegisterAggFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    name: *const c_char,
    args: i32,
    context: usize,
    init: InitAggFunction,
    step: StepFunction,
    finalize: FinalizeFunction,
    context_destructor: Option<ContextDestructor>,
    aggregate_destructor: Option<ContextDestructor>,
    value_destructor: Option<ValueDestructor>,
) -> ResultCode;

pub type InitAggFunction = unsafe extern "C" fn(context: usize) -> *mut AggCtx;
pub type StepFunction =
    unsafe extern "C" fn(context: usize, ctx: *mut AggCtx, argc: i32, argv: *const Value) -> Value;
pub type FinalizeFunction = unsafe extern "C" fn(context: usize, ctx: *mut AggCtx) -> Value;

#[repr(C)]
pub struct AggCtx {
    pub state: *mut c_void,
}

pub trait AggFunc {
    type State: Default;
    type Error: Display;
    const NAME: &'static str;
    const ARGS: i32;

    fn step(state: &mut Self::State, args: &[Value]);
    fn finalize(state: Self::State) -> Result<Value, Self::Error>;
}

/// A scalar function that carries opaque, per-registration state.
///
/// `State` is constructed once per registration via [`ScalarFunc::init`], shared
/// by reference across every invocation, and dropped when the function is
/// unregistered or the owning connection is dropped. Because a single registration
/// is shared across connections and may be invoked concurrently, `State` must be
/// `Send + Sync`.
///
/// Stateless functions should use the `#[scalar]` attribute macro instead.
pub trait ScalarFunc {
    type State: Send + Sync;
    const NAME: &'static str;
    const ALIAS: Option<&'static str> = None;

    fn init() -> Self::State;
    fn call(state: &Self::State, args: &[Value]) -> Value;
}
