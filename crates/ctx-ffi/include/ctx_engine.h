#ifndef CTX_ENGINE_H
#define CTX_ENGINE_H

#pragma once

#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

/**
 * Opaque cooperative cancellation token owned by C callers.
 */
typedef struct CtxCancel CtxCancel;

/**
 * Opaque context engine handle owned by C callers.
 */
typedef struct CtxEngine CtxEngine;

/**
 * Create a new context engine with one allowed filesystem root.
 *
 * Returns null if `root` is null, not UTF-8, cannot be canonicalized, or if a
 * panic is caught before the engine is created.
 *
 * # Safety
 * `root` must be either null or a valid NUL-terminated C string for the
 * duration of the call.
 */
struct CtxEngine *ctx_engine_new(const char *root);

/**
 * Create a cooperative cancellation token for cancellable requests.
 *
 * The caller owns the returned token and must release it exactly once with
 * `ctx_cancel_free`. Returns null if a panic is caught.
 *
 * # Safety
 * This function has no pointer arguments and is safe to call from C. It is
 * marked unsafe only because it is part of the raw C ABI surface.
 */
struct CtxCancel *ctx_cancel_new(void);

/**
 * Request cancellation for a token created by `ctx_cancel_new`.
 *
 * Passing null is allowed. This function may be called concurrently from
 * another thread while `ctx_engine_handle_request_cancellable` is running; the
 * token is backed by `Arc<AtomicBool>` and uses atomic load/store operations.
 *
 * # Safety
 * `cancel` must be null or a pointer returned by `ctx_cancel_new` that has not
 * been freed. The caller must not free `cancel` concurrently with this call.
 */
void ctx_cancel_trigger(struct CtxCancel *cancel);

/**
 * Free a cancellation token returned by `ctx_cancel_new`.
 *
 * Passing null is allowed. Do not free a token while another thread is passing
 * the same pointer to `ctx_engine_handle_request_cancellable` or
 * `ctx_cancel_trigger`.
 *
 * # Safety
 * `cancel` must be null or a pointer previously returned by `ctx_cancel_new`
 * that has not already been freed.
 */
void ctx_cancel_free(struct CtxCancel *cancel);

/**
 * Handle one JSON tool-call request and return a newly allocated JSON string.
 *
 * The request shape is the same MCP `tools/call` params object used by
 * `ctx-mcp`, for example `{"name":"read_file","arguments":{"path":"README.md"}}`.
 * This is not a full JSON-RPC envelope; JSON-RPC lifecycle remains owned by
 * `ctx-mcp`.
 *
 * The caller must release each non-null return value exactly once with
 * `ctx_engine_free_string`. Do not release returned strings with `free(3)` and
 * do not use them after release.
 *
 * The same engine may be used by concurrent request calls as long as no thread
 * calls `ctx_engine_free` until all active requests have returned. Returns null
 * if `eng` or `req_json` is null, `req_json` is not UTF-8, a response cannot be
 * converted into a C string, or if a panic is caught. Normal dispatch failures
 * are returned as JSON: `{"error":{"kind":"...","message":"..."}}`.
 *
 * # Safety
 * `eng` must be a non-null pointer returned by `ctx_engine_new` that has not
 * been freed. `req_json` must be a valid NUL-terminated C string for the
 * duration of the call. The caller must not free `eng` concurrently with this
 * call.
 */
char *ctx_engine_handle_request(struct CtxEngine *eng, const char *req_json);

/**
 * Handle one JSON tool-call request with optional cooperative cancellation.
 *
 * `cancel` may be null, which means the request cannot be cancelled. If a
 * non-null token is triggered while a long search or repo-map is running, the
 * request returns JSON: `{"error":{"kind":"cancelled",...}}`.
 *
 * The caller may call `ctx_cancel_trigger` on the same token from another
 * thread while this function is running. The token uses `Arc<AtomicBool>`, so
 * concurrent cancellation is atomic-safe. The caller must keep `cancel` alive
 * until this function returns.
 *
 * # Safety
 * `eng` must be a non-null pointer returned by `ctx_engine_new` that has not
 * been freed. `req_json` must be a valid NUL-terminated C string for the
 * duration of the call. `cancel` must be null or a pointer returned by
 * `ctx_cancel_new` that remains alive for the duration of the call. The caller
 * must not free `eng` concurrently with this call.
 */
char *ctx_engine_handle_request_cancellable(struct CtxEngine *eng,
                                            const char *req_json,
                                            const struct CtxCancel *cancel);

/**
 * Invalidate cached filesystem snapshots and codemap entries for this engine.
 *
 * Passing null is allowed. Active requests are not canceled; they may continue
 * using the snapshot they already obtained. Later requests rebuild cache entries
 * on demand.
 *
 * # Safety
 * `eng` must be null or a pointer returned by `ctx_engine_new` that has not
 * been freed. The caller must not free `eng` concurrently with this call.
 */
void ctx_engine_invalidate(struct CtxEngine *eng);

/**
 * Free a string returned by `ctx_engine_handle_request` or
 * `ctx_engine_handle_request_cancellable`.
 *
 * Returned strings must be released exactly once with this function, not with
 * `free(3)`. Passing null is allowed.
 *
 * # Safety
 * `value` must be null or a pointer previously returned by
 * `ctx_engine_handle_request` or `ctx_engine_handle_request_cancellable` that
 * has not already been freed. The caller must
 * not use `value` after this call returns.
 */
void ctx_engine_free_string(char *value);

/**
 * Free an engine returned by `ctx_engine_new`.
 *
 * Engine handles must be released exactly once with this function, not with
 * `free(3)`. Passing null is allowed.
 *
 * # Safety
 * `eng` must be null or a pointer previously returned by `ctx_engine_new` that
 * has not already been freed. No request may be active on this engine, and no
 * returned strings may be in active use by the caller after the engine is freed.
 */
void ctx_engine_free(struct CtxEngine *eng);

#endif  /* CTX_ENGINE_H */
