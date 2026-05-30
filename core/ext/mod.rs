#[cfg(feature = "fs")]
mod dynamic;
mod vtab_xconnect;
use crate::index_method::backing_btree::BackingBtreeIndexMethod;
#[cfg(all(feature = "fts", not(target_family = "wasm")))]
use crate::index_method::fts::{FtsIndexMethod, FTS_INDEX_METHOD_NAME};
use crate::index_method::toy_vector_sparse_ivf::VectorSparseInvertedIndexMethod;
use crate::index_method::{
    BACKING_BTREE_INDEX_METHOD_NAME, TOY_VECTOR_SPARSE_IVF_INDEX_METHOD_NAME,
};
use crate::schema::{Schema, Table};
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::sync::Mutex;
#[cfg(all(target_os = "linux", feature = "io_uring", not(miri)))]
use crate::UringIO;
#[cfg(all(target_os = "windows", feature = "experimental_win_iocp", not(miri)))]
use crate::WindowsIOCP;

use crate::{function::ExternalFunc, Connection, Database};
use crate::{vtab::VirtualTable, SymbolTable};
#[cfg(feature = "fs")]
use crate::{LimboError, IO};
#[cfg(feature = "fs")]
pub use dynamic::{add_builtin_vfs_extensions, add_vfs_module, list_vfs_modules, VfsMod};
use std::{
    ffi::{c_char, c_void, CStr, CString},
    sync::Arc,
};
use turso_ext::{
    ContextDestructor, ExtensionApi, GeometryCallbackFn, InitAggFunction, ResultCode,
    ScalarFunction, ScalarFunctionConnCtx, VTabKind, VTabModuleImpl, ValueDestructor,
};

/// Wrapper for scalar functions that also receive a connection context.
/// Stores the user-provided context alongside the connection reference so the
/// C callback can forward both to the Rust layer.
struct ScalarWithCtx {
    /// Packed `(geom_fn_ptr as usize << 16 | user_context)` — enough for both pointers.
    packed: usize,
    _conn: std::marker::PhantomData<Arc<Connection>>,
}

/// Registry of per-connection scalar functions that receive a connection context.
/// Keyed by connection pointer identity (Arc::as_ptr as usize) → function name → wrapper.
/// Uses `LazyLock` to avoid const-evaluation limitations.
static SCALAR_WITH_CTX_REGISTRY: std::sync::LazyLock<
    std::sync::RwLock<
        std::collections::HashMap<usize, std::collections::HashMap<String, ScalarWithCtx>>,
    >,
> = std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));

/// Unregister all functions associated with a given connection.
/// Called when the connection is closed.
pub(crate) fn unregister_connection_functions(conn: &Arc<Connection>) {
    let mut registry = SCALAR_WITH_CTX_REGISTRY.write().unwrap();
    let ptr = Arc::as_ptr(conn);
    let ptr_usize = ptr as *const _ as usize;
    registry.remove(&ptr_usize);
}

/// Set the current thread's connection context for `ScalarWithCtx` callbacks.
/// This is called once per SQL function invocation before dispatching.
pub(crate) fn set_current_conn_ctx(conn: Arc<Connection>) {
    CURRENT_CONN_CTX.with(|ctx| ctx.replace(Some(conn)));
}

/// Clear the current thread's connection context.
pub(crate) fn clear_current_conn_ctx() {
    CURRENT_CONN_CTX.with(|ctx| ctx.replace(None));
}

pub(crate) fn get_current_conn_ctx() -> Option<Arc<Connection>> {
    CURRENT_CONN_CTX.with(|ctx| ctx.borrow().clone())
}
/// Used by `ScalarWithCtx` callbacks to recover the connection without passing it
/// through the C FFI boundary explicitly.
///
/// RefCell rather than Cell so we can call .borrow() without needing T: Copy.
thread_local! {
    static CURRENT_CONN_CTX: std::cell::RefCell<Option<Arc<Connection>>> = std::cell::RefCell::new(None);
}
pub use turso_ext::Value;
pub use turso_ext::{FinalizeFunction, StepFunction, Value as ExtValue, ValueType as ExtValueType};
pub use vtab_xconnect::{execute, prepare_stmt};

/// The context passed to extensions to register with Core
/// along with the function pointers
#[repr(C)]
pub struct ExtensionCtx {
    syms: *mut SymbolTable,
    schema: *mut c_void,
    /// We must bump the prepare context generation so prepared statements
    /// know they need to be reprepared after extension registration.
    prepare_context_generation: *const AtomicU64,
}

pub(crate) unsafe extern "C" fn register_vtab_module(
    ctx: *mut c_void,
    name: *const c_char,
    module: VTabModuleImpl,
    kind: VTabKind,
) -> ResultCode {
    if name.is_null() || ctx.is_null() {
        return ResultCode::Error;
    }

    let c_str = unsafe { CString::from_raw(name as *mut c_char) };
    let name_str = match c_str.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return ResultCode::Error,
    };

    let ext_ctx = unsafe { &mut *(ctx as *mut ExtensionCtx) };
    let module = Arc::new(module);
    let vmodule = VTabImpl {
        module_kind: kind,
        implementation: module,
    };

    unsafe {
        let syms = &mut *ext_ctx.syms;
        syms.vtab_modules.insert(name_str.clone(), vmodule.into());
        if !ext_ctx.prepare_context_generation.is_null() {
            (*ext_ctx.prepare_context_generation).fetch_add(1, Ordering::Release);
        }

        if kind == VTabKind::TableValuedFunction {
            if let Ok(vtab) = VirtualTable::function(&name_str, syms) {
                let table = Arc::new(Table::Virtual(vtab));
                let mutex = &*(ext_ctx.schema as *mut Mutex<Arc<Schema>>);
                let mut guard = mutex.lock();
                let schema = Arc::make_mut(&mut *guard);
                schema.tables.insert(name_str, table);
            } else {
                return ResultCode::Error;
            }
        }
    }
    ResultCode::OK
}

#[derive(Clone)]
pub struct VTabImpl {
    pub module_kind: VTabKind,
    pub implementation: Arc<VTabModuleImpl>,
}

pub(crate) unsafe fn register_scalar_function(
    ctx: *mut c_void,
    name: *const c_char,
    func: ScalarFunction,
) -> ResultCode {
    unsafe { register_scalar_function_with_options(ctx, name, -1, false, 0, func, None, None) }
}

pub(crate) unsafe extern "C" fn register_scalar_function_with_options(
    ctx: *mut c_void,
    name: *const c_char,
    argc: i32,
    deterministic: bool,
    context: usize,
    callback: ScalarFunction,
    context_destructor: Option<ContextDestructor>,
    value_destructor: Option<ValueDestructor>,
) -> ResultCode {
    if ctx.is_null() || name.is_null() || argc < -1 {
        return ResultCode::InvalidArgs;
    }
    let c_str = unsafe { CStr::from_ptr(name) };
    let name_str = match c_str.to_str() {
        Ok(s) => crate::util::normalize_ident(s),
        Err(_) => return ResultCode::InvalidArgs,
    };
    let ext_ctx = unsafe { &mut *(ctx as *mut ExtensionCtx) };
    unsafe {
        (*ext_ctx.syms).functions.insert(
            name_str.clone(),
            Arc::new(ExternalFunc::new_scalar(
                name_str,
                argc,
                deterministic,
                context,
                callback,
                context_destructor,
                value_destructor,
            )),
        );
        if !ext_ctx.prepare_context_generation.is_null() {
            (*ext_ctx.prepare_context_generation).fetch_add(1, Ordering::Release);
        }
    }
    ResultCode::OK
}

pub(crate) unsafe extern "C" fn register_scalar_function_with_ctx(
    ctx: *mut c_void,
    name: *const c_char,
    argc: i32,
    deterministic: bool,
    context: usize,
    callback: unsafe extern "C" fn(
        context: usize,
        conn: ScalarFunctionConnCtx,
        argc: i32,
        argv: *const Value,
        context_destructor: Option<ContextDestructor>,
        value_destructor: Option<ValueDestructor>,
    ) -> Value,
    context_destructor: Option<ContextDestructor>,
    value_destructor: Option<ValueDestructor>,
) -> ResultCode {
    if ctx.is_null() || name.is_null() || argc < -1 {
        return ResultCode::InvalidArgs;
    }
    let c_str = unsafe { CStr::from_ptr(name) };
    let name_str = match c_str.to_str() {
        Ok(s) => crate::util::normalize_ident(s),
        Err(_) => return ResultCode::InvalidArgs,
    };
    // The connection pointer (conn) is not known at registration time for static extensions —
    // it's passed at invocation time in the per-connection registry.
    // We register a C shim that extracts the connection from thread-local state and dispatches.
    let ext_ctx = unsafe { &mut *(ctx as *mut ExtensionCtx) };
    unsafe {
        (*ext_ctx.syms).functions.insert(
            name_str.clone(),
            Arc::new(ExternalFunc::new_scalar_with_ctx(
                name_str,
                argc,
                deterministic,
                context,
                callback,
                context_destructor,
                value_destructor,
            )),
        );
        if !ext_ctx.prepare_context_generation.is_null() {
            (*ext_ctx.prepare_context_generation).fetch_add(1, Ordering::Release);
        }
    }
    ResultCode::OK
}

/// Register an rtree geometry callback function on a connection.
/// This is the entry point for `sqlite3_rtree_geometry_callback`.
///
/// The callback is registered as a SQL scalar function that produces a serialized
/// MATCH blob when invoked. The blob encodes the callback pointer and user context,
/// which `filter()` deserializes to invoke the geometry function per-cell.
///
/// The `geom_fn` is stored in the per-connection `SCALAR_WITH_CTX_REGISTRY` and
/// dispatched through a shim that recovers the connection from `CURRENT_CONN_CTX`.
pub(crate) unsafe extern "C" fn rtree_geometry_callback(
    ctx: *mut c_void,
    name: *const c_char,
    geom_fn: GeometryCallbackFn,
    user_context: usize,
) -> ResultCode {
    use crate::Value;

    if ctx.is_null() || name.is_null() {
        return ResultCode::Error;
    }
    let c_str = unsafe { CStr::from_ptr(name) };
    let name_str = match c_str.to_str() {
        Ok(s) => crate::util::normalize_ident(s),
        Err(_) => return ResultCode::Error,
    };

    // Geometry callbacks are registered per-connection via the WITH_CTX mechanism.
    // We get the connection from CURRENT_CONN_CTX at invocation time (set by the VDBE
    // before calling any SQL function). We store the raw (geom_fn, user_context) in
    // the registry; a dispatch shim retrieves them and calls geom_fn.
    let mut registry = SCALAR_WITH_CTX_REGISTRY.write().unwrap();
    let conn_ptr = (CURRENT_CONN_CTX
        .with(|ctx| ctx.borrow().as_ref().map(|c| Arc::as_ptr(c) as usize)))
    .unwrap_or(0usize);

    if conn_ptr == 0 {
        return ResultCode::Error;
    }

    let conn_funcs = registry.entry(conn_ptr).or_default();
    // Pack both pointers into one usize (shift geom_fn to high bits, keep user_context in low bits).
    // On 64-bit this gives plenty of space for both.
    let packed = (geom_fn as usize) << 16 | (user_context & ((1 << 16) - 1));
    conn_funcs.insert(
        name_str.clone(),
        ScalarWithCtx {
            packed,
            _conn: std::marker::PhantomData,
        },
    );

    // Register a C shim that, when called, reads CURRENT_CONN_CTX, looks up the
    // (geom_fn, user_context) from the registry, calls geom_fn(n_dim, coords, n_param,
    // params, user_context, result), and returns a blob that the rtree MATCH filter decodes.
    unsafe extern "C" fn geom_shim(
        _context: usize,
        _conn: ScalarFunctionConnCtx,
        argc: i32,
        argv: *const ExtValue,
        _context_destructor: Option<ContextDestructor>,
        _value_destructor: Option<ValueDestructor>,
    ) -> ExtValue {
        let args = std::slice::from_raw_parts(argv, argc as usize);

        // Look up (geom_fn, user_context) from CURRENT_CONN_CTX's registry.
        let (geom_fn, user_context) = match CURRENT_CONN_CTX.with(|ctx| ctx.borrow().clone()) {
            Some(conn) => {
                let conn_ptr = Arc::as_ptr(&conn) as *const _ as usize;
                let registry = SCALAR_WITH_CTX_REGISTRY.read().unwrap();
                match registry.get(&conn_ptr).and_then(|m| m.get("")) {
                    Some(swctx) => {
                        let geom_fn_ptr = swctx.packed >> 16;
                        let geom_fn: GeometryCallbackFn =
                            unsafe { std::mem::transmute(geom_fn_ptr) };
                        let ctx = swctx.packed & ((1 << 16) - 1);
                        (geom_fn, ctx)
                    }
                    None => return ExtValue::null(),
                }
            }
            None => return ExtValue::null(),
        };

        // Pack into a MATCH blob: [4 iSize][8 geom_ptr][8 ctx_ptr][8 nParam][params...]
        // geom_shim params come from SQL function args as f64 values.
        let mut blob = Vec::with_capacity(4 + 8 + 8 + 8 + args.len() * 8);

        // iSize placeholder (fix up after)
        let size_placeholder = blob.len();
        blob.extend_from_slice(&(0u32).to_le_bytes());

        // geom_ptr
        blob.extend_from_slice(&(geom_fn as usize as u64).to_le_bytes());

        // context ptr
        blob.extend_from_slice(&(user_context as u64).to_le_bytes());

        // nParam
        blob.extend_from_slice(&(args.len() as u64).to_le_bytes());

        // params: SQL function arguments as f64 values
        for arg in args {
            blob.extend_from_slice(&arg.to_float().unwrap_or(0.0).to_le_bytes());
        }

        // Fix up iSize
        let actual_size = blob.len() as u32;
        blob[size_placeholder..size_placeholder + 4].copy_from_slice(&actual_size.to_le_bytes());

        ExtValue::from_blob(blob)
    }

    // Insert the shim as a regular scalar function with ctx.
    let ext_ctx = unsafe { &mut *(ctx as *mut ExtensionCtx) };
    unsafe {
        (*ext_ctx.syms).functions.insert(
            name_str.clone(),
            Arc::new(ExternalFunc::new_scalar_with_ctx(
                name_str,
                -1,
                false,
                user_context,
                geom_shim,
                None,
                None,
            )),
        );
    }
    ResultCode::OK
}

pub(crate) unsafe extern "C" fn unregister_function(
    ctx: *mut c_void,
    name: *const c_char,
) -> ResultCode {
    if ctx.is_null() || name.is_null() {
        return ResultCode::InvalidArgs;
    }
    let c_str = unsafe { CStr::from_ptr(name) };
    let name_str = match c_str.to_str() {
        Ok(s) => crate::util::normalize_ident(s),
        Err(_) => return ResultCode::InvalidArgs,
    };
    let ext_ctx = unsafe { &mut *(ctx as *mut ExtensionCtx) };
    unsafe {
        if (*ext_ctx.syms).functions.remove(&name_str).is_none() {
            return ResultCode::NotFound;
        }
        if !ext_ctx.prepare_context_generation.is_null() {
            (*ext_ctx.prepare_context_generation).fetch_add(1, Ordering::Release);
        }
    }
    ResultCode::OK
}

pub(crate) unsafe extern "C" fn register_aggregate_function(
    ctx: *mut c_void,
    name: *const c_char,
    args: i32,
    context: usize,
    init_func: InitAggFunction,
    step_func: StepFunction,
    finalize_func: FinalizeFunction,
    context_destructor: Option<ContextDestructor>,
    aggregate_destructor: Option<ContextDestructor>,
    value_destructor: Option<ValueDestructor>,
) -> ResultCode {
    if ctx.is_null() || name.is_null() || args < -1 {
        return ResultCode::InvalidArgs;
    }
    let c_str = unsafe { CStr::from_ptr(name) };
    let name_str = match c_str.to_str() {
        Ok(s) => crate::util::normalize_ident(s),
        Err(_) => return ResultCode::InvalidArgs,
    };
    let ext_ctx = unsafe { &mut *(ctx as *mut ExtensionCtx) };
    unsafe {
        (*ext_ctx.syms).functions.insert(
            name_str.clone(),
            Arc::new(ExternalFunc::new_aggregate(
                name_str,
                args,
                context,
                (init_func, step_func, finalize_func),
                context_destructor,
                aggregate_destructor,
                value_destructor,
            )),
        );
        if !ext_ctx.prepare_context_generation.is_null() {
            (*ext_ctx.prepare_context_generation).fetch_add(1, Ordering::Release);
        }
    }
    ResultCode::OK
}

impl Database {
    #[cfg(feature = "fs")]
    #[allow(clippy::arc_with_non_send_sync, dead_code)]
    pub fn open_with_vfs(
        &self,
        path: &str,
        vfs: &str,
    ) -> crate::Result<(Arc<dyn IO>, Arc<Database>)> {
        use crate::{MemoryIO, SyscallIO};
        use dynamic::get_vfs_modules;

        let io: Arc<dyn IO> = match vfs {
            "memory" => Arc::new(MemoryIO::new()),
            #[cfg(feature = "io_memory_yield")]
            "memory_yield" => Arc::new(crate::MemoryYieldIO::new()),
            "syscall" => Arc::new(SyscallIO::new()?),
            #[cfg(all(target_os = "linux", feature = "io_uring", not(miri)))]
            "io_uring" => Arc::new(UringIO::new()?),
            #[cfg(all(target_os = "windows", feature = "experimental_win_iocp", not(miri)))]
            "experimental_win_iocp" => Arc::new(WindowsIOCP::new()?),
            other => match get_vfs_modules().iter().find(|v| v.0 == vfs) {
                Some((_, vfs)) => vfs.clone(),
                None => {
                    return Err(LimboError::InvalidArgument(format!("no such VFS: {other}")));
                }
            },
        };
        let db = Self::open_file(io.clone(), path)?;
        Ok((io, db))
    }

    /// Register any built-in extensions that can be stored on the Database so we do not have
    /// to register these once-per-connection, and the connection can just extend its symbol table
    pub fn register_global_builtin_extensions(&self) -> Result<(), String> {
        {
            let mut syms = self.builtin_syms.write();
            syms.index_methods.insert(
                TOY_VECTOR_SPARSE_IVF_INDEX_METHOD_NAME.to_string(),
                Arc::new(VectorSparseInvertedIndexMethod),
            );
            syms.index_methods.insert(
                BACKING_BTREE_INDEX_METHOD_NAME.to_string(),
                Arc::new(BackingBtreeIndexMethod),
            );
            #[cfg(all(feature = "fts", not(target_family = "wasm")))]
            syms.index_methods
                .insert(FTS_INDEX_METHOD_NAME.to_string(), Arc::new(FtsIndexMethod));
        }
        let syms = self.builtin_syms.data_ptr();
        // Pass the mutex pointer and the appropriate handler
        let schema_mutex_ptr =
            &*self.schema as *const Mutex<Arc<Schema>> as *mut Mutex<Arc<Schema>>;
        let ctx = Box::into_raw(Box::new(ExtensionCtx {
            syms,
            schema: schema_mutex_ptr as *mut c_void,
            prepare_context_generation: std::ptr::null(),
        }));
        #[allow(unused)]
        let mut ext_api = ExtensionApi {
            ctx: ctx as *mut c_void,
            register_scalar_function: register_scalar_function_with_options,
            register_scalar_function_with_ctx: register_scalar_function_with_ctx,
            register_aggregate_function,
            unregister_function,
            register_vtab_module,
            rtree_geometry_callback,
            #[cfg(feature = "fs")]
            vfs_interface: turso_ext::VfsInterface {
                register_vfs: dynamic::register_vfs,
                builtin_vfs: std::ptr::null_mut(),
                builtin_vfs_count: 0,
            },
        };

        #[cfg(feature = "uuid")]
        crate::uuid::register_extension(&mut ext_api);
        #[cfg(feature = "series")]
        crate::series::register_extension(&mut ext_api);
        #[cfg(feature = "time")]
        crate::time::register_extension(&mut ext_api);
        #[cfg(feature = "percentile")]
        crate::percentile::register_extension(&mut ext_api);
        crate::regexp::register_extension(&mut ext_api);
        #[cfg(feature = "rtree")]
        {
            // SAFETY: limbo_rtree has no global state and is safe to register
            unsafe { crate::limbo_rtree::register_extension(&mut ext_api) };
        }
        #[cfg(feature = "fs")]
        {
            let vfslist = add_builtin_vfs_extensions(Some(ext_api)).map_err(|e| e.to_string())?;
            for (name, vfs) in vfslist {
                add_vfs_module(name, vfs);
            }
        }
        let _ = unsafe { Box::from_raw(ctx) };
        Ok(())
    }
}

impl Connection {
    /// Build the connection's extension api context for manually registering an extension.
    /// you probably want to use `Connection::load_extension(path)`.
    ///
    /// # Safety
    /// Only to be used when registering a staticly linked extension manually.
    /// You should only ever call this method on your applications startup,
    /// The caller is responsible for calling `_free_extension_ctx` after registering the
    /// extension.
    ///
    /// usage:
    /// ```ignore
    /// let ext_api = conn._build_turso_ext();
    /// unsafe {
    ///     my_extension::register_extension(&mut ext_api);
    ///     conn._free_extension_ctx(ext_api);
    /// }
    ///```
    pub unsafe fn _build_turso_ext(&self) -> ExtensionApi {
        let schema_mutex_ptr =
            &*self.db.schema as *const Mutex<Arc<Schema>> as *mut Mutex<Arc<Schema>>;
        let ctx = ExtensionCtx {
            syms: self.syms.data_ptr(),
            schema: schema_mutex_ptr as *mut c_void,
            prepare_context_generation: &self.prepare_context_generation as *const _,
        };
        let ctx = Box::into_raw(Box::new(ctx)) as *mut c_void;
        ExtensionApi {
            ctx,
            register_scalar_function: register_scalar_function_with_options,
            register_scalar_function_with_ctx: register_scalar_function_with_ctx,
            register_aggregate_function,
            unregister_function,
            register_vtab_module,
            rtree_geometry_callback,
            #[cfg(feature = "fs")]
            vfs_interface: turso_ext::VfsInterface {
                register_vfs: dynamic::register_vfs,
                builtin_vfs: std::ptr::null_mut(),
                builtin_vfs_count: 0,
            },
        }
    }

    /// Free the connection's extension libary context after registering an extension manually.
    /// # Safety
    /// Only to be used if you have previously called Connection::build_turso_ext
    pub unsafe fn _free_extension_ctx(&self, api: ExtensionApi) {
        if api.ctx.is_null() {
            return;
        }
        let _ = unsafe { Box::from_raw(api.ctx as *mut ExtensionCtx) };
    }
}
