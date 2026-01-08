//! System utilities for process debugging and introspection.
//!
//! Similar to Erlang's `sys` module, this provides tools for:
//! - **Suspend/Resume**: Pause and unpause message processing
//! - **Statistics**: Track message counts, call times
//! - **Tracing**: Log all messages in/out of a process
//!
//! # Example
//!
//! ```ignore
//! use spawned_concurrency::{sys, HasPid};
//!
//! let mut handle = MyServer::new().start(Backend::Async);
//! let pid = handle.pid();
//!
//! // Enable statistics collection
//! sys::statistics(pid, true);
//!
//! // Do some work...
//! handle.call(SomeMessage).await;
//!
//! // Get statistics
//! if let Some(stats) = sys::get_statistics(pid) {
//!     println!("Calls: {}, Casts: {}", stats.call_count, stats.cast_count);
//! }
//!
//! // Suspend the process (stops processing messages)
//! sys::suspend(pid);
//!
//! // Resume processing
//! sys::resume(pid);
//! ```

use crate::pid::Pid;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// Statistics collected for a process.
#[derive(Debug, Clone, Default)]
pub struct Statistics {
    /// Number of call messages received.
    pub call_count: u64,
    /// Number of cast messages received.
    pub cast_count: u64,
    /// Number of info/system messages received.
    pub info_count: u64,
    /// Total number of messages processed.
    pub total_messages: u64,
    /// Time when statistics collection started.
    pub started_at: Option<Instant>,
    /// Total time spent processing messages.
    pub processing_time: Duration,
    /// Number of errors encountered.
    pub error_count: u64,
}

impl Statistics {
    fn new() -> Self {
        Self {
            started_at: Some(Instant::now()),
            ..Default::default()
        }
    }

    /// Record a call message.
    pub fn record_call(&mut self, duration: Duration) {
        self.call_count += 1;
        self.total_messages += 1;
        self.processing_time += duration;
    }

    /// Record a cast message.
    pub fn record_cast(&mut self, duration: Duration) {
        self.cast_count += 1;
        self.total_messages += 1;
        self.processing_time += duration;
    }

    /// Record an info/system message.
    pub fn record_info(&mut self, duration: Duration) {
        self.info_count += 1;
        self.total_messages += 1;
        self.processing_time += duration;
    }

    /// Record an error.
    pub fn record_error(&mut self) {
        self.error_count += 1;
    }

    /// Get the uptime since statistics collection started.
    pub fn uptime(&self) -> Duration {
        self.started_at
            .map(|t| t.elapsed())
            .unwrap_or(Duration::ZERO)
    }

    /// Get the average message processing time.
    pub fn avg_processing_time(&self) -> Duration {
        if self.total_messages == 0 {
            Duration::ZERO
        } else {
            self.processing_time / self.total_messages as u32
        }
    }
}

/// Trace options for a process.
#[derive(Debug, Clone, Default)]
pub struct TraceOptions {
    /// Log all incoming call messages.
    pub trace_calls: bool,
    /// Log all incoming cast messages.
    pub trace_casts: bool,
    /// Log all info/system messages.
    pub trace_info: bool,
    /// Log all outgoing replies.
    pub trace_replies: bool,
}

impl TraceOptions {
    /// Enable all tracing.
    pub fn all() -> Self {
        Self {
            trace_calls: true,
            trace_casts: true,
            trace_info: true,
            trace_replies: true,
        }
    }

    /// Disable all tracing.
    pub fn none() -> Self {
        Self::default()
    }

    /// Check if any tracing is enabled.
    pub fn is_enabled(&self) -> bool {
        self.trace_calls || self.trace_casts || self.trace_info || self.trace_replies
    }
}

/// Internal state for sys module.
struct SysState {
    /// Suspended processes.
    suspended: HashMap<Pid, bool>,
    /// Statistics per process.
    statistics: HashMap<Pid, Statistics>,
    /// Trace options per process.
    trace_options: HashMap<Pid, TraceOptions>,
}

impl SysState {
    fn new() -> Self {
        Self {
            suspended: HashMap::new(),
            statistics: HashMap::new(),
            trace_options: HashMap::new(),
        }
    }
}

/// Global sys state.
static SYS_STATE: std::sync::LazyLock<RwLock<SysState>> =
    std::sync::LazyLock::new(|| RwLock::new(SysState::new()));

// ==================== Suspend/Resume ====================

/// Suspend a process.
///
/// A suspended process will not process any new messages until resumed.
/// Messages sent to a suspended process are queued.
///
/// Note: The process must check `is_suspended()` in its message loop
/// for this to take effect. Built-in GenServer support is automatic.
pub fn suspend(pid: Pid) {
    let mut state = SYS_STATE.write().unwrap();
    state.suspended.insert(pid, true);
    tracing::debug!(%pid, "Process suspended");
}

/// Resume a suspended process.
///
/// The process will continue processing queued messages.
pub fn resume(pid: Pid) {
    let mut state = SYS_STATE.write().unwrap();
    state.suspended.remove(&pid);
    tracing::debug!(%pid, "Process resumed");
}

/// Check if a process is suspended.
pub fn is_suspended(pid: Pid) -> bool {
    let state = SYS_STATE.read().unwrap();
    state.suspended.get(&pid).copied().unwrap_or(false)
}

// ==================== Statistics ====================

/// Enable or disable statistics collection for a process.
///
/// When enabled, the process will track message counts and processing times.
pub fn statistics(pid: Pid, enable: bool) {
    let mut state = SYS_STATE.write().unwrap();
    if enable {
        // Use Statistics::new() instead of default() to set started_at
        #[allow(clippy::unwrap_or_default)]
        state.statistics.entry(pid).or_insert_with(Statistics::new);
        tracing::debug!(%pid, "Statistics enabled");
    } else {
        state.statistics.remove(&pid);
        tracing::debug!(%pid, "Statistics disabled");
    }
}

/// Check if statistics collection is enabled for a process.
pub fn statistics_enabled(pid: Pid) -> bool {
    let state = SYS_STATE.read().unwrap();
    state.statistics.contains_key(&pid)
}

/// Get statistics for a process.
///
/// Returns `None` if statistics collection is not enabled.
pub fn get_statistics(pid: Pid) -> Option<Statistics> {
    let state = SYS_STATE.read().unwrap();
    state.statistics.get(&pid).cloned()
}

/// Reset statistics for a process.
///
/// Clears all counters but keeps statistics collection enabled.
pub fn reset_statistics(pid: Pid) {
    let mut state = SYS_STATE.write().unwrap();
    if state.statistics.contains_key(&pid) {
        state.statistics.insert(pid, Statistics::new());
    }
}

/// Record a call message (internal use).
#[doc(hidden)]
pub fn record_call(pid: Pid, duration: Duration) {
    let mut state = SYS_STATE.write().unwrap();
    if let Some(stats) = state.statistics.get_mut(&pid) {
        stats.record_call(duration);
    }
}

/// Record a cast message (internal use).
#[doc(hidden)]
pub fn record_cast(pid: Pid, duration: Duration) {
    let mut state = SYS_STATE.write().unwrap();
    if let Some(stats) = state.statistics.get_mut(&pid) {
        stats.record_cast(duration);
    }
}

/// Record an info message (internal use).
#[doc(hidden)]
pub fn record_info(pid: Pid, duration: Duration) {
    let mut state = SYS_STATE.write().unwrap();
    if let Some(stats) = state.statistics.get_mut(&pid) {
        stats.record_info(duration);
    }
}

/// Record an error (internal use).
#[doc(hidden)]
pub fn record_error(pid: Pid) {
    let mut state = SYS_STATE.write().unwrap();
    if let Some(stats) = state.statistics.get_mut(&pid) {
        stats.record_error();
    }
}

// ==================== Tracing ====================

/// Enable or disable tracing for a process.
///
/// When enabled, all messages will be logged via the `tracing` crate.
pub fn trace(pid: Pid, enable: bool) {
    let mut state = SYS_STATE.write().unwrap();
    if enable {
        state
            .trace_options
            .entry(pid)
            .or_insert_with(TraceOptions::all);
        tracing::debug!(%pid, "Tracing enabled");
    } else {
        state.trace_options.remove(&pid);
        tracing::debug!(%pid, "Tracing disabled");
    }
}

/// Set specific trace options for a process.
pub fn set_trace_options(pid: Pid, options: TraceOptions) {
    let mut state = SYS_STATE.write().unwrap();
    if options.is_enabled() {
        state.trace_options.insert(pid, options);
    } else {
        state.trace_options.remove(&pid);
    }
}

/// Get trace options for a process.
pub fn get_trace_options(pid: Pid) -> Option<TraceOptions> {
    let state = SYS_STATE.read().unwrap();
    state.trace_options.get(&pid).cloned()
}

/// Check if tracing is enabled for a process.
pub fn tracing_enabled(pid: Pid) -> bool {
    let state = SYS_STATE.read().unwrap();
    state
        .trace_options
        .get(&pid)
        .map(|o| o.is_enabled())
        .unwrap_or(false)
}

/// Log a call message if tracing is enabled (internal use).
#[doc(hidden)]
pub fn trace_call<T: std::fmt::Debug>(pid: Pid, message: &T) {
    let state = SYS_STATE.read().unwrap();
    if let Some(opts) = state.trace_options.get(&pid) {
        if opts.trace_calls {
            tracing::info!(%pid, message = ?message, "CALL");
        }
    }
}

/// Log a cast message if tracing is enabled (internal use).
#[doc(hidden)]
pub fn trace_cast<T: std::fmt::Debug>(pid: Pid, message: &T) {
    let state = SYS_STATE.read().unwrap();
    if let Some(opts) = state.trace_options.get(&pid) {
        if opts.trace_casts {
            tracing::info!(%pid, message = ?message, "CAST");
        }
    }
}

/// Log an info message if tracing is enabled (internal use).
#[doc(hidden)]
pub fn trace_info<T: std::fmt::Debug>(pid: Pid, message: &T) {
    let state = SYS_STATE.read().unwrap();
    if let Some(opts) = state.trace_options.get(&pid) {
        if opts.trace_info {
            tracing::info!(%pid, message = ?message, "INFO");
        }
    }
}

/// Log a reply if tracing is enabled (internal use).
#[doc(hidden)]
pub fn trace_reply<T: std::fmt::Debug>(pid: Pid, reply: &T) {
    let state = SYS_STATE.read().unwrap();
    if let Some(opts) = state.trace_options.get(&pid) {
        if opts.trace_replies {
            tracing::info!(%pid, reply = ?reply, "REPLY");
        }
    }
}

// ==================== Cleanup ====================

/// Clean up sys state for a terminated process.
///
/// Called automatically when a process terminates.
pub fn cleanup(pid: Pid) {
    let mut state = SYS_STATE.write().unwrap();
    state.suspended.remove(&pid);
    state.statistics.remove(&pid);
    state.trace_options.remove(&pid);
}

/// Clear all sys state (for testing).
#[cfg(test)]
pub fn clear() {
    let mut state = SYS_STATE.write().unwrap();
    state.suspended.clear();
    state.statistics.clear();
    state.trace_options.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_suspend_resume() {
        clear();
        let pid = Pid::new();

        assert!(!is_suspended(pid));

        suspend(pid);
        assert!(is_suspended(pid));

        resume(pid);
        assert!(!is_suspended(pid));
    }

    #[test]
    fn test_statistics() {
        clear();
        let pid = Pid::new();

        // Not enabled by default
        assert!(!statistics_enabled(pid));
        assert!(get_statistics(pid).is_none());

        // Enable
        statistics(pid, true);
        assert!(statistics_enabled(pid));

        // Record some stats
        record_call(pid, Duration::from_millis(10));
        record_call(pid, Duration::from_millis(20));
        record_cast(pid, Duration::from_millis(5));
        record_info(pid, Duration::from_millis(2));
        record_error(pid);

        let stats = get_statistics(pid).unwrap();
        assert_eq!(stats.call_count, 2);
        assert_eq!(stats.cast_count, 1);
        assert_eq!(stats.info_count, 1);
        assert_eq!(stats.total_messages, 4);
        assert_eq!(stats.error_count, 1);
        assert_eq!(stats.processing_time, Duration::from_millis(37));

        // Reset
        reset_statistics(pid);
        let stats = get_statistics(pid).unwrap();
        assert_eq!(stats.call_count, 0);
        assert_eq!(stats.total_messages, 0);

        // Disable
        statistics(pid, false);
        assert!(!statistics_enabled(pid));
    }

    #[test]
    fn test_tracing() {
        clear();
        let pid = Pid::new();

        // Not enabled by default
        assert!(!tracing_enabled(pid));
        assert!(get_trace_options(pid).is_none());

        // Enable all
        trace(pid, true);
        assert!(tracing_enabled(pid));
        let opts = get_trace_options(pid).unwrap();
        assert!(opts.trace_calls);
        assert!(opts.trace_casts);

        // Set specific options
        set_trace_options(
            pid,
            TraceOptions {
                trace_calls: true,
                trace_casts: false,
                trace_info: false,
                trace_replies: true,
            },
        );
        let opts = get_trace_options(pid).unwrap();
        assert!(opts.trace_calls);
        assert!(!opts.trace_casts);
        assert!(opts.trace_replies);

        // Disable
        trace(pid, false);
        assert!(!tracing_enabled(pid));
    }

    #[test]
    fn test_cleanup() {
        clear();
        let pid = Pid::new();

        suspend(pid);
        statistics(pid, true);
        trace(pid, true);

        assert!(is_suspended(pid));
        assert!(statistics_enabled(pid));
        assert!(tracing_enabled(pid));

        cleanup(pid);

        assert!(!is_suspended(pid));
        assert!(!statistics_enabled(pid));
        assert!(!tracing_enabled(pid));
    }

    #[test]
    fn test_statistics_avg_processing_time() {
        let mut stats = Statistics::new();

        // No messages yet
        assert_eq!(stats.avg_processing_time(), Duration::ZERO);

        // Add some messages
        stats.record_call(Duration::from_millis(100));
        stats.record_call(Duration::from_millis(200));
        stats.record_cast(Duration::from_millis(100));

        // Average should be 400ms / 4 = 100ms
        // Wait, 3 messages not 4
        // 100 + 200 + 100 = 400ms / 3 = 133.33ms
        let avg = stats.avg_processing_time();
        assert!(avg >= Duration::from_millis(130) && avg <= Duration::from_millis(140));
    }
}
