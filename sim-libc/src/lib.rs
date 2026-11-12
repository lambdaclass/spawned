//! Clock and entropy interposers for Spawned's deterministic simulator.
//!
//! **Simulation builds only.** The whole crate is `#![cfg(spawned_sim)]`: in
//! any other build it compiles to nothing, so the unsafe code below never
//! exists outside `--cfg spawned_sim`. `spawned-rt` depends on it only under
//! that cfg and keeps `#![forbid(unsafe_code)]` in every build; this crate is
//! where the unsafe FFI the technique needs is contained.
//!
//! The technique is madsim's: the test binary defines the libc symbols that
//! read the clock and the OS entropy source (`clock_gettime`,
//! `gettimeofday`, `getrandom`, `getentropy`, and on Linux the `syscall`
//! entry for `SYS_getrandom`; `CCRandomGenerateBytes` on macOS). References
//! from Rust code (std's `Instant::now`, `SystemTime::now`, `HashMap`'s
//! `RandomState` keys, the `getrandom` 0.2 crate behind `rand::thread_rng`
//! and `OsRng`, `chrono::Utc::now`) bind to these definitions at link time.
//! Each one asks the [`Hooks`] that `spawned-rt` installed: on a thread that
//! runs a simulated world, the hook answers with virtual time or seeded bytes;
//! on every other thread (and for the clock inside [`real_clock`]) the call goes to the
//! real libc function (found with `dlsym(RTLD_NEXT)`).
//!
//! Supported targets: Linux with glibc (x86_64 and aarch64 for the `syscall`
//! entry) and macOS. Elsewhere [`SUPPORTED`] is false and nothing is
//! interposed. Not covered: code that finds `getrandom` with
//! `dlsym(RTLD_DEFAULT)` at run time (the `getrandom` 0.3+ crate on Linux,
//! that is `rand` 0.9+), unless the binary exports the symbol
//! (`-C link-arg=-Wl,--export-dynamic-symbol=getrandom`); raw `syscall`
//! instructions; and the vDSO called directly.
//!
//! Caveats: the interposers are not async-signal-safe (they read
//! thread-locals, clone an `Arc`, and in strict mode lock a mutex and capture
//! a backtrace), so a signal handler that reads the clock on a simulated
//! thread can deadlock. The `syscall` entry sits in front of every
//! `libc::syscall` call in the binary (std's futex and `statx` among them);
//! numbers other than `SYS_getrandom` go straight to libc. Only one copy of
//! this crate (and no other interposer such as madsim's) may be linked: two
//! would define the same symbols.
#![cfg(spawned_sim)]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]
// Without interposers (unsupported targets) the helpers below go unused.
#![cfg_attr(
    not(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")),
    allow(dead_code)
)]

use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Duration;

/// Which clock a read asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clock {
    /// `CLOCK_MONOTONIC` and its variants (`Instant::now`): time since boot.
    Monotonic,
    /// `CLOCK_REALTIME` and `gettimeofday` (`SystemTime::now`): time since
    /// the Unix epoch.
    Realtime,
}

/// Callbacks `spawned-rt` installs. They run inside a libc call, so they must
/// not panic (a panic is caught and the real function used) and must not hold
/// a lock that a clock read on the same thread could need; reads they make
/// themselves go to the real functions.
#[derive(Debug, Clone, Copy)]
pub struct Hooks {
    /// A clock read: the time to report (since boot for
    /// [`Clock::Monotonic`], since the Unix epoch for [`Clock::Realtime`]),
    /// or `None` for the real clock. The `&str` names the call.
    pub clock: fn(Clock, &'static str) -> Option<Duration>,
    /// An entropy read: fill the buffer and return `true`, or `false` for
    /// real bytes. The `&str` names the call; the flag marks reads std makes
    /// for `HashMap` keys (`GRND_INSECURE`, `CCRandomGenerateBytes`), which a
    /// strict mode should not report.
    pub random: fn(&mut [u8], &'static str, bool) -> bool,
}

/// Whether this target has interposers. Where it is false, [`install`] does
/// nothing and every read is real.
pub const SUPPORTED: bool = cfg!(any(
    all(target_os = "linux", target_env = "gnu"),
    target_os = "macos"
));

static HOOKS: AtomicPtr<Hooks> = AtomicPtr::new(std::ptr::null_mut());

/// Route intercepted reads through `hooks` from now on (process-wide;
/// idempotent).
pub fn install(hooks: &'static Hooks) {
    HOOKS.store(hooks as *const Hooks as *mut Hooks, Ordering::Release);
    imp::ensure_linked();
}

thread_local! {
    /// Set while this thread runs a hook, so the hook's own reads (a lock,
    /// a backtrace) go to libc instead of recursing. A const `Cell` has no
    /// destructor, so it is readable during thread teardown.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
    /// Nonzero inside [`real_clock`]: clock reads go to libc.
    static REAL_CLOCK: Cell<u32> = const { Cell::new(0) };
}

struct InHook;
impl InHook {
    /// `None` if this thread is already inside a hook (or tearing down).
    fn enter() -> Option<Self> {
        let entered = IN_HOOK.try_with(|h| !h.replace(true));
        // `then`, not `then_some`: a guard built and dropped on the refusing
        // path would clear the flag.
        entered.ok()?.then(|| Self)
    }
}
impl Drop for InHook {
    fn drop(&mut self) {
        let _ = IN_HOOK.try_with(|h| h.set(false));
    }
}

/// Run `f` with the real clock on this thread (the simulator's own
/// wall-clock watchdog and turn budget, and Tokio's runtime metrics around a
/// simulated poll, use it). Entropy reads are still seeded: a `HashMap`
/// created here must not take this thread's keys from the OS.
pub fn real_clock<R>(f: impl FnOnce() -> R) -> R {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = REAL_CLOCK.try_with(|d| d.set(d.get().saturating_sub(1)));
        }
    }
    let _ = REAL_CLOCK.try_with(|d| d.set(d.get() + 1));
    let _guard = Guard;
    f()
}

/// Run `f` with the clock interposed again, inside a [`real_clock`] (the
/// simulator polls a task, whose reads are the program's, from inside Tokio
/// machinery whose own reads are not).
pub fn intercepted<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(u32);
    impl Drop for Restore {
        fn drop(&mut self) {
            let depth = self.0;
            let _ = REAL_CLOCK.try_with(|d| d.set(depth));
        }
    }
    let depth = REAL_CLOCK.try_with(|d| d.replace(0)).unwrap_or(0);
    let _restore = Restore(depth);
    f()
}

/// Ask the installed hooks, unless none are installed or this thread is
/// already inside one.
fn ask<R>(f: impl FnOnce(&Hooks) -> Option<R>) -> Option<R> {
    let hooks = HOOKS.load(Ordering::Acquire);
    if hooks.is_null() {
        return None;
    }
    let _in_hook = InHook::enter()?;
    // SAFETY: `HOOKS` only ever holds a `&'static Hooks` (see `install`).
    let hooks = unsafe { &*hooks };
    catch_unwind(AssertUnwindSafe(|| f(hooks))).ok().flatten()
}

fn clock(kind: Clock, call: &'static str) -> Option<Duration> {
    if REAL_CLOCK.try_with(|d| d.get() > 0).unwrap_or(true) {
        return None;
    }
    ask(|hooks| (hooks.clock)(kind, call))
}

/// Fill `len` bytes at `buf` from the hooks; `false` means use the real call.
///
/// # Safety
/// `buf` must be valid for writes of `len` bytes.
unsafe fn random(buf: *mut u8, len: usize, call: &'static str, quiet: bool) -> bool {
    if buf.is_null() && len > 0 {
        return false;
    }
    ask(|hooks| {
        let bytes: &mut [u8] = if len == 0 {
            &mut []
        } else {
            // SAFETY: the caller guarantees `buf` is valid for `len` writes;
            // zeroing it first makes every byte initialized before the slice
            // is formed.
            unsafe {
                std::ptr::write_bytes(buf, 0, len);
                std::slice::from_raw_parts_mut(buf, len)
            }
        };
        (hooks.random)(bytes, call, quiet).then_some(())
    })
    .is_some()
}

fn to_timespec(t: Duration) -> libc::timespec {
    libc::timespec {
        tv_sec: t.as_secs() as libc::time_t,
        tv_nsec: t.subsec_nanos() as _,
    }
}

#[cfg(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos"))]
mod imp {
    use super::{clock, random, to_timespec, Clock};
    use libc::{c_int, c_void, clockid_t, size_t, timespec, timeval};
    use std::ffi::CStr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Address of the real `name` (the next definition after this binary's,
    /// in libc), cached in `cache`. Aborts if there is none: every interposed
    /// symbol exists in the platform libc.
    fn real(cache: &AtomicUsize, name: &CStr) -> usize {
        let mut addr = cache.load(Ordering::Relaxed);
        if addr == 0 {
            // SAFETY: `name` is a valid C string; RTLD_NEXT is a valid handle.
            addr = unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr()) } as usize;
            if addr == 0 {
                // SAFETY: abort has no preconditions.
                unsafe { libc::abort() };
            }
            cache.store(addr, Ordering::Relaxed);
        }
        addr
    }

    fn classify(clk: clockid_t) -> Option<(Clock, &'static str)> {
        #[cfg(target_os = "linux")]
        let kind = match clk {
            libc::CLOCK_REALTIME | libc::CLOCK_REALTIME_COARSE | libc::CLOCK_TAI => Clock::Realtime,
            libc::CLOCK_MONOTONIC
            | libc::CLOCK_MONOTONIC_RAW
            | libc::CLOCK_MONOTONIC_COARSE
            | libc::CLOCK_BOOTTIME => Clock::Monotonic,
            _ => return None,
        };
        #[cfg(target_os = "macos")]
        let kind = match clk {
            libc::CLOCK_REALTIME => Clock::Realtime,
            libc::CLOCK_MONOTONIC | libc::CLOCK_MONOTONIC_RAW | libc::CLOCK_UPTIME_RAW => {
                Clock::Monotonic
            }
            _ => return None,
        };
        Some(match kind {
            Clock::Realtime => (kind, "clock_gettime(CLOCK_REALTIME)"),
            Clock::Monotonic => (kind, "clock_gettime(CLOCK_MONOTONIC)"),
        })
    }

    /// Interposed `clock_gettime`: virtual time for the monotonic and
    /// realtime clocks on a simulated thread.
    ///
    /// # Safety
    /// Same contract as libc's `clock_gettime`.
    #[no_mangle]
    pub unsafe extern "C" fn clock_gettime(clk: clockid_t, tp: *mut timespec) -> c_int {
        if !tp.is_null() {
            if let Some((kind, call)) = classify(clk) {
                if let Some(t) = clock(kind, call) {
                    // SAFETY: `tp` is non-null and, per the libc contract,
                    // valid for writes.
                    unsafe { tp.write(to_timespec(t)) };
                    return 0;
                }
            }
        }
        static REAL: AtomicUsize = AtomicUsize::new(0);
        let addr = real(&REAL, c"clock_gettime");
        // SAFETY: `addr` is libc's `clock_gettime`, which has this signature.
        let f: unsafe extern "C" fn(clockid_t, *mut timespec) -> c_int =
            unsafe { std::mem::transmute(addr) };
        // SAFETY: forwarding the caller's arguments unchanged.
        unsafe { f(clk, tp) }
    }

    /// Interposed `gettimeofday`: virtual realtime on a simulated thread.
    ///
    /// # Safety
    /// Same contract as libc's `gettimeofday`.
    #[no_mangle]
    pub unsafe extern "C" fn gettimeofday(tv: *mut timeval, tz: *mut c_void) -> c_int {
        if !tv.is_null() {
            if let Some(t) = clock(Clock::Realtime, "gettimeofday") {
                // SAFETY: `tv` is non-null and valid for writes per the
                // libc contract.
                unsafe {
                    tv.write(timeval {
                        tv_sec: t.as_secs() as libc::time_t,
                        tv_usec: t.subsec_micros() as _,
                    })
                };
                return 0;
            }
        }
        static REAL: AtomicUsize = AtomicUsize::new(0);
        let addr = real(&REAL, c"gettimeofday");
        // SAFETY: `addr` is libc's `gettimeofday`, which has this signature.
        let f: unsafe extern "C" fn(*mut timeval, *mut c_void) -> c_int =
            unsafe { std::mem::transmute(addr) };
        // SAFETY: forwarding the caller's arguments unchanged.
        unsafe { f(tv, tz) }
    }

    /// Interposed `getentropy`: seeded bytes on a simulated thread.
    ///
    /// # Safety
    /// Same contract as libc's `getentropy`.
    #[no_mangle]
    pub unsafe extern "C" fn getentropy(buf: *mut c_void, len: size_t) -> c_int {
        // SAFETY: the libc contract makes `buf` valid for `len` writes.
        if len <= 256 && unsafe { random(buf.cast(), len, "getentropy", false) } {
            return 0;
        }
        static REAL: AtomicUsize = AtomicUsize::new(0);
        let addr = real(&REAL, c"getentropy");
        // SAFETY: `addr` is libc's `getentropy`, which has this signature.
        let f: unsafe extern "C" fn(*mut c_void, size_t) -> c_int =
            unsafe { std::mem::transmute(addr) };
        // SAFETY: forwarding the caller's arguments unchanged.
        unsafe { f(buf, len) }
    }

    /// `GRND_INSECURE`: std asks for `HashMap` keys with it.
    #[cfg(target_os = "linux")]
    const GRND_INSECURE: libc::c_uint = 0x0004;

    /// Interposed `getrandom` (std's `HashMap` keys and `std::random` bind
    /// to it through a weak symbol): seeded bytes on a simulated thread.
    ///
    /// # Safety
    /// Same contract as libc's `getrandom`.
    #[cfg(target_os = "linux")]
    #[no_mangle]
    pub unsafe extern "C" fn getrandom(
        buf: *mut c_void,
        len: size_t,
        flags: libc::c_uint,
    ) -> libc::ssize_t {
        let quiet = flags & GRND_INSECURE != 0;
        // SAFETY: the libc contract makes `buf` valid for `len` writes.
        if unsafe { random(buf.cast(), len, "getrandom", quiet) } {
            return len as libc::ssize_t;
        }
        static REAL: AtomicUsize = AtomicUsize::new(0);
        let addr = real(&REAL, c"getrandom");
        // SAFETY: `addr` is libc's `getrandom`, which has this signature.
        let f: unsafe extern "C" fn(*mut c_void, size_t, libc::c_uint) -> libc::ssize_t =
            unsafe { std::mem::transmute(addr) };
        // SAFETY: forwarding the caller's arguments unchanged.
        unsafe { f(buf, len, flags) }
    }

    /// Interposed `syscall`, for the `getrandom` crate 0.2 (behind
    /// `rand::thread_rng` and `OsRng` in rand 0.8), which calls
    /// `syscall(SYS_getrandom, ..)`. Every other number goes straight to
    /// libc. Defined with six fixed arguments: on x86_64 and aarch64 Linux a
    /// variadic call passes integer arguments exactly like a fixed one, and
    /// reading an argument the caller did not pass yields an unused value
    /// (libc's own `syscall` reads six too).
    ///
    /// # Safety
    /// Same contract as libc's `syscall`.
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[no_mangle]
    pub unsafe extern "C" fn syscall(
        n: libc::c_long,
        a1: libc::c_long,
        a2: libc::c_long,
        a3: libc::c_long,
        a4: libc::c_long,
        a5: libc::c_long,
        a6: libc::c_long,
    ) -> libc::c_long {
        if n == libc::SYS_getrandom {
            let quiet = (a3 as libc::c_uint) & GRND_INSECURE != 0;
            // SAFETY: for SYS_getrandom, `a1` is a buffer valid for `a2`
            // writes per the syscall's contract.
            if a2 >= 0
                && unsafe { random(a1 as *mut u8, a2 as usize, "syscall(SYS_getrandom)", quiet) }
            {
                return a2;
            }
        }
        static REAL: AtomicUsize = AtomicUsize::new(0);
        let addr = real(&REAL, c"syscall");
        // SAFETY: `addr` is libc's variadic `syscall`.
        let f: unsafe extern "C" fn(libc::c_long, ...) -> libc::c_long =
            unsafe { std::mem::transmute(addr) };
        // SAFETY: forwarding the caller's arguments (and at worst unused
        // values in the trailing slots) unchanged.
        unsafe { f(n, a1, a2, a3, a4, a5, a6) }
    }

    /// Interposed `CCRandomGenerateBytes` (std's `HashMap` keys on macOS).
    ///
    /// # Safety
    /// Same contract as CommonCrypto's `CCRandomGenerateBytes`.
    #[cfg(target_os = "macos")]
    #[no_mangle]
    pub unsafe extern "C" fn CCRandomGenerateBytes(buf: *mut c_void, len: size_t) -> i32 {
        // SAFETY: the contract makes `buf` valid for `len` writes.
        if unsafe { random(buf.cast(), len, "CCRandomGenerateBytes", true) } {
            return 0;
        }
        static REAL: AtomicUsize = AtomicUsize::new(0);
        let addr = real(&REAL, c"CCRandomGenerateBytes");
        // SAFETY: `addr` is CommonCrypto's function, which has this signature.
        let f: unsafe extern "C" fn(*mut c_void, size_t) -> i32 =
            unsafe { std::mem::transmute(addr) };
        // SAFETY: forwarding the caller's arguments unchanged.
        unsafe { f(buf, len) }
    }

    /// Reference every interposer so the linker keeps the object that
    /// defines them (and so their definitions win over libc's).
    #[inline(never)]
    pub(super) fn ensure_linked() {
        std::hint::black_box([
            clock_gettime as *const (),
            gettimeofday as *const (),
            getentropy as *const (),
        ]);
        #[cfg(target_os = "linux")]
        std::hint::black_box(getrandom as *const ());
        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        std::hint::black_box(syscall as *const ());
        #[cfg(target_os = "macos")]
        std::hint::black_box(CCRandomGenerateBytes as *const ());
    }
}

#[cfg(not(any(all(target_os = "linux", target_env = "gnu"), target_os = "macos")))]
mod imp {
    pub(super) fn ensure_linked() {}
}
