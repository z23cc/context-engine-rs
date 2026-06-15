//! Minimal C ABI for linking the context engine into native hosts.

use ctx_core::{
    CancelToken, FsCatalogProvider, RootPolicy, ScanOptions, dispatch_error_json,
    dispatch_error_kind, handle_tool_call_json_cancellable,
};
use std::{
    ffi::{CStr, CString},
    os::raw::c_char,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    ptr,
};

/// Opaque context engine handle owned by C callers.
pub struct CtxEngine {
    provider: FsCatalogProvider,
}

/// Opaque cooperative cancellation token owned by C callers.
pub struct CtxCancel {
    token: CancelToken,
}

/// Create a new context engine with one allowed filesystem root.
///
/// Returns null if `root` is null, not UTF-8, cannot be canonicalized, or if a
/// panic is caught before the engine is created.
///
/// # Safety
/// `root` must be either null or a valid NUL-terminated C string for the
/// duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ctx_engine_new(root: *const c_char) -> *mut CtxEngine {
    catch_unwind(AssertUnwindSafe(|| {
        let root = match c_string_to_str(root) {
            Some(root) => root,
            None => return ptr::null_mut(),
        };
        let policy = match RootPolicy::new(vec![PathBuf::from(root)]) {
            Ok(policy) => policy,
            Err(_) => return ptr::null_mut(),
        };
        let engine = CtxEngine {
            provider: FsCatalogProvider::new(policy, ScanOptions::default()),
        };
        Box::into_raw(Box::new(engine))
    }))
    .unwrap_or(ptr::null_mut())
}

/// Create a cooperative cancellation token for cancellable requests.
///
/// The caller owns the returned token and must release it exactly once with
/// `ctx_cancel_free`. Returns null if a panic is caught.
///
/// # Safety
/// This function has no pointer arguments and is safe to call from C. It is
/// marked unsafe only because it is part of the raw C ABI surface.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ctx_cancel_new() -> *mut CtxCancel {
    catch_unwind(AssertUnwindSafe(|| {
        Box::into_raw(Box::new(CtxCancel {
            token: CancelToken::new(),
        }))
    }))
    .unwrap_or(ptr::null_mut())
}

/// Request cancellation for a token created by `ctx_cancel_new`.
///
/// Passing null is allowed. This function may be called concurrently from
/// another thread while `ctx_engine_handle_request_cancellable` is running; the
/// token is backed by `Arc<AtomicBool>` and uses atomic load/store operations.
///
/// # Safety
/// `cancel` must be null or a pointer returned by `ctx_cancel_new` that has not
/// been freed. The caller must not free `cancel` concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ctx_cancel_trigger(cancel: *mut CtxCancel) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(cancel) = unsafe { cancel.as_ref() } {
            cancel.token.cancel();
        }
    }));
}

/// Free a cancellation token returned by `ctx_cancel_new`.
///
/// Passing null is allowed. Do not free a token while another thread is passing
/// the same pointer to `ctx_engine_handle_request_cancellable` or
/// `ctx_cancel_trigger`.
///
/// # Safety
/// `cancel` must be null or a pointer previously returned by `ctx_cancel_new`
/// that has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ctx_cancel_free(cancel: *mut CtxCancel) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !cancel.is_null() {
            let _ = unsafe { Box::from_raw(cancel) };
        }
    }));
}

/// Handle one JSON tool-call request and return a newly allocated JSON string.
///
/// The request shape is the same MCP `tools/call` params object used by
/// `ctx-mcp`, for example `{"name":"read_file","arguments":{"path":"README.md"}}`.
/// This is not a full JSON-RPC envelope; JSON-RPC lifecycle remains owned by
/// `ctx-mcp`.
///
/// The caller must release each non-null return value exactly once with
/// `ctx_engine_free_string`. Do not release returned strings with `free(3)` and
/// do not use them after release.
///
/// The same engine may be used by concurrent request calls as long as no thread
/// calls `ctx_engine_free` until all active requests have returned. Returns null
/// if `eng` or `req_json` is null, `req_json` is not UTF-8, a response cannot be
/// converted into a C string, or if a panic is caught. Normal dispatch failures
/// are returned as JSON: `{"error":{"kind":"...","message":"..."}}`.
///
/// # Safety
/// `eng` must be a non-null pointer returned by `ctx_engine_new` that has not
/// been freed. `req_json` must be a valid NUL-terminated C string for the
/// duration of the call. The caller must not free `eng` concurrently with this
/// call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ctx_engine_handle_request(
    eng: *mut CtxEngine,
    req_json: *const c_char,
) -> *mut c_char {
    unsafe { ctx_engine_handle_request_cancellable(eng, req_json, ptr::null()) }
}

/// Handle one JSON tool-call request with optional cooperative cancellation.
///
/// `cancel` may be null, which means the request cannot be cancelled. If a
/// non-null token is triggered while a long search or repo-map is running, the
/// request returns JSON: `{"error":{"kind":"cancelled",...}}`.
///
/// The caller may call `ctx_cancel_trigger` on the same token from another
/// thread while this function is running. The token uses `Arc<AtomicBool>`, so
/// concurrent cancellation is atomic-safe. The caller must keep `cancel` alive
/// until this function returns.
///
/// # Safety
/// `eng` must be a non-null pointer returned by `ctx_engine_new` that has not
/// been freed. `req_json` must be a valid NUL-terminated C string for the
/// duration of the call. `cancel` must be null or a pointer returned by
/// `ctx_cancel_new` that remains alive for the duration of the call. The caller
/// must not free `eng` concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ctx_engine_handle_request_cancellable(
    eng: *mut CtxEngine,
    req_json: *const c_char,
    cancel: *const CtxCancel,
) -> *mut c_char {
    catch_unwind(AssertUnwindSafe(|| {
        let engine = match unsafe { eng.as_ref() } {
            Some(engine) => engine,
            None => return ptr::null_mut(),
        };
        let request = match c_string_to_str(req_json) {
            Some(request) => request,
            None => return ptr::null_mut(),
        };
        let never_cancel;
        let cancel_token = match unsafe { cancel.as_ref() } {
            Some(cancel) => &cancel.token,
            None => {
                never_cancel = CancelToken::never();
                &never_cancel
            }
        };
        let response =
            match handle_tool_call_json_cancellable(&engine.provider, request, cancel_token) {
                Ok(response) => response,
                Err(err) => dispatch_error_json(dispatch_error_kind(&err), &err.to_string()),
            };
        string_to_c(response)
    }))
    .unwrap_or(ptr::null_mut())
}

/// Invalidate cached filesystem snapshots and codemap entries for this engine.
///
/// Passing null is allowed. Active requests are not canceled; they may continue
/// using the snapshot they already obtained. Later requests rebuild cache entries
/// on demand.
///
/// # Safety
/// `eng` must be null or a pointer returned by `ctx_engine_new` that has not
/// been freed. The caller must not free `eng` concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ctx_engine_invalidate(eng: *mut CtxEngine) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(engine) = unsafe { eng.as_ref() } {
            engine.provider.invalidate();
        }
    }));
}

/// Free a string returned by `ctx_engine_handle_request` or
/// `ctx_engine_handle_request_cancellable`.
///
/// Returned strings must be released exactly once with this function, not with
/// `free(3)`. Passing null is allowed.
///
/// # Safety
/// `value` must be null or a pointer previously returned by
/// `ctx_engine_handle_request` or `ctx_engine_handle_request_cancellable` that
/// has not already been freed. The caller must
/// not use `value` after this call returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ctx_engine_free_string(value: *mut c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !value.is_null() {
            let _ = unsafe { CString::from_raw(value) };
        }
    }));
}

/// Free an engine returned by `ctx_engine_new`.
///
/// Engine handles must be released exactly once with this function, not with
/// `free(3)`. Passing null is allowed.
///
/// # Safety
/// `eng` must be null or a pointer previously returned by `ctx_engine_new` that
/// has not already been freed. No request may be active on this engine, and no
/// returned strings may be in active use by the caller after the engine is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ctx_engine_free(eng: *mut CtxEngine) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !eng.is_null() {
            let _ = unsafe { Box::from_raw(eng) };
        }
    }));
}

fn c_string_to_str<'a>(value: *const c_char) -> Option<&'a str> {
    if value.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(value) }.to_str().ok()
}

fn string_to_c(value: String) -> *mut c_char {
    match CString::new(value) {
        Ok(value) => value.into_raw(),
        Err(_) => ptr::null_mut(),
    }
}
