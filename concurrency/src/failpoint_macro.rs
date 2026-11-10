//! Internal `fail_point!` call-site macro.
//!
//! With the `failpoints` feature, each call evaluates a named site in
//! [`crate::failpoints`]. Without it, every form expands to `()`: the site
//! name, the error expression and the evaluation are not compiled, so default
//! builds contain no failpoint code or strings (mirrors `lifecycle_trace`).
//!
//! Forms:
//! - `fail_point!("site")`: synchronous; may panic or sleep the thread.
//! - `fail_point!("site", async)`: may panic, sleep asynchronously or yield.
//! - `fail_point!("site" => expr)` / `fail_point!("site", async => expr)`:
//!   additionally evaluates `expr` (typically `return Err(..)`) when the site
//!   is configured with `Action::Error`.

#[cfg(feature = "failpoints")]
macro_rules! fail_point {
    ($name:literal) => {{
        let _ = $crate::failpoints::eval_sync($name);
    }};
    ($name:literal => $on_error:expr) => {{
        if $crate::failpoints::eval_sync($name) {
            $on_error;
        }
    }};
    ($name:literal, async) => {{
        let _ = $crate::failpoints::eval_async($name).await;
    }};
    ($name:literal, async => $on_error:expr) => {{
        if $crate::failpoints::eval_async($name).await {
            $on_error;
        }
    }};
}

#[cfg(not(feature = "failpoints"))]
macro_rules! fail_point {
    ($name:literal) => {};
    ($name:literal => $on_error:expr) => {};
    ($name:literal, async) => {};
    ($name:literal, async => $on_error:expr) => {};
}
