mod functions;
mod types;
#[cfg(feature = "vfs")]
mod vfs_modules;
mod vtabs;
pub use functions::{
    AggCtx, AggFunc, ContextDestructor, FinalizeFunction, GeometryCallbackFn, InitAggFunction,
    ScalarFunc, ScalarFunction, ScalarFunctionConnCtx, StepFunction, ValueDestructor,
};
use functions::{RegisterAggFn, RegisterScalarFn, RegisterScalarFnWithCtx, UnregisterFunctionFn};
use std::os::raw::{c_char, c_void};
#[cfg(feature = "vfs")]
pub use turso_macros::VfsDerive;
pub use turso_macros::{
    register_extension, scalar, AggregateDerive, ScalarDerive, VTabModuleDerive,
};
pub use types::{ResultCode, StepResult, Value, ValueType};
#[cfg(feature = "vfs")]
pub use vfs_modules::{
    BufferRef, Callback, IOCallback, RegisterVfsFn, SendPtr, VfsExtension, VfsFile, VfsFileImpl,
    VfsImpl, VfsInterface,
};
use vtabs::RegisterModuleFn;
pub use vtabs::{
    Conn, Connection, ConstraintInfo, ConstraintOp, ConstraintUsage, ExtIndexInfo, IndexInfo,
    OrderByInfo, Statement, Stmt, VTabCreateResult, VTabCursor, VTabKind, VTabModule,
    VTabModuleImpl, VTable,
};

pub type ExtResult<T> = std::result::Result<T, ResultCode>;

pub type ExtensionEntryPoint = unsafe extern "C" fn(api: *const ExtensionApi) -> ResultCode;

#[repr(C)]
pub struct ExtensionApi {
    pub ctx: *mut c_void,
    pub register_scalar_function: RegisterScalarFn,
    pub register_scalar_function_with_ctx: RegisterScalarFnWithCtx,
    pub register_aggregate_function: RegisterAggFn,
    pub unregister_function: UnregisterFunctionFn,
    pub register_vtab_module: RegisterModuleFn,
    /// Entry point for `sqlite3_rtree_geometry_callback`. Takes a connection context
    /// pointer, function name, and an opaque user context pointer. The connection is
    /// recovered from `CURRENT_CONN_CTX` at invocation time; the user context is
    /// passed through to the geometry callback.
    pub rtree_geometry_callback: unsafe extern "C" fn(
        ctx: *mut c_void,
        name: *const c_char,
        func: GeometryCallbackFn,
        user_context: usize,
    ) -> ResultCode,
    #[cfg(feature = "vfs")]
    pub vfs_interface: VfsInterface,
}

unsafe impl Send for ExtensionApi {}
unsafe impl Send for ExtensionApiRef {}

#[repr(C)]
pub struct ExtensionApiRef {
    pub api: *const ExtensionApi,
}
