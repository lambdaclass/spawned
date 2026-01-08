//! Global process table for tracking links and monitors.
//!
//! This module provides the infrastructure for process linking and monitoring.
//! It maintains a global table of:
//! - Active links between processes
//! - Active monitors
//! - Message senders for delivering system messages
//! - Process exit trapping configuration

use crate::link::MonitorRef;
use crate::pid::{ExitReason, Pid};
use crate::registry;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

/// Trait for sending system messages to a process.
///
/// This is implemented by the internal message sender that can deliver
/// SystemMessage to a GenServer's mailbox.
pub trait SystemMessageSender: Send + Sync {
    /// Send a DOWN message (from a monitored process).
    fn send_down(&self, pid: Pid, monitor_ref: MonitorRef, reason: ExitReason);

    /// Send an EXIT message (from a linked process).
    fn send_exit(&self, pid: Pid, reason: ExitReason);

    /// Kill this process (when linked process crashes and not trapping exits).
    fn kill(&self, reason: ExitReason);

    /// Check if the process is still alive.
    fn is_alive(&self) -> bool;
}

/// Entry for a registered process in the table.
struct ProcessEntry {
    /// Sender for system messages.
    sender: Arc<dyn SystemMessageSender>,
    /// Whether this process traps exits.
    trap_exit: bool,
}

/// Global process table.
///
/// This is a singleton that tracks all active processes, their links, and monitors.
struct ProcessTableInner {
    /// All registered processes.
    processes: HashMap<Pid, ProcessEntry>,

    /// Bidirectional links: pid -> set of linked pids.
    links: HashMap<Pid, HashSet<Pid>>,

    /// Active monitors: monitor_ref -> (monitoring_pid, monitored_pid).
    monitors: HashMap<MonitorRef, (Pid, Pid)>,

    /// Reverse lookup: pid -> set of monitor refs watching this pid.
    monitored_by: HashMap<Pid, HashSet<MonitorRef>>,
}

impl ProcessTableInner {
    fn new() -> Self {
        Self {
            processes: HashMap::new(),
            links: HashMap::new(),
            monitors: HashMap::new(),
            monitored_by: HashMap::new(),
        }
    }
}

/// Global process table instance.
static PROCESS_TABLE: std::sync::LazyLock<RwLock<ProcessTableInner>> =
    std::sync::LazyLock::new(|| RwLock::new(ProcessTableInner::new()));

/// Register a process with the table.
///
/// Called when a GenServer starts.
pub fn register(pid: Pid, sender: Arc<dyn SystemMessageSender>) {
    let mut table = PROCESS_TABLE.write().unwrap();
    table.processes.insert(
        pid,
        ProcessEntry {
            sender,
            trap_exit: false,
        },
    );
}

/// Unregister a process from the table.
///
/// Called when a GenServer terminates. Also cleans up links, monitors, and registry.
pub fn unregister(pid: Pid, reason: ExitReason) {
    // First, notify linked and monitoring processes
    notify_exit(pid, reason);

    // Clean up the registry (remove any registered name for this pid)
    registry::unregister_pid(pid);

    // Then clean up the table
    let mut table = PROCESS_TABLE.write().unwrap();

    // Remove from processes
    table.processes.remove(&pid);

    // Clean up links (remove from all linked processes)
    if let Some(linked_pids) = table.links.remove(&pid) {
        for linked_pid in linked_pids {
            if let Some(other_links) = table.links.get_mut(&linked_pid) {
                other_links.remove(&pid);
            }
        }
    }

    // Clean up monitors where this pid was the monitored process
    if let Some(refs) = table.monitored_by.remove(&pid) {
        for monitor_ref in refs {
            table.monitors.remove(&monitor_ref);
        }
    }

    // Clean up monitors where this pid was the monitoring process
    let refs_to_remove: Vec<MonitorRef> = table
        .monitors
        .iter()
        .filter(|(_, (monitoring_pid, _))| *monitoring_pid == pid)
        .map(|(ref_, _)| *ref_)
        .collect();

    for ref_ in refs_to_remove {
        if let Some((_, monitored_pid)) = table.monitors.remove(&ref_) {
            if let Some(refs) = table.monitored_by.get_mut(&monitored_pid) {
                refs.remove(&ref_);
            }
        }
    }
}

/// Notify linked and monitoring processes of an exit.
fn notify_exit(pid: Pid, reason: ExitReason) {
    let table = PROCESS_TABLE.read().unwrap();

    // Notify linked processes
    if let Some(linked_pids) = table.links.get(&pid) {
        for &linked_pid in linked_pids {
            if let Some(entry) = table.processes.get(&linked_pid) {
                if entry.trap_exit {
                    // Send EXIT message
                    entry.sender.send_exit(pid, reason.clone());
                } else if !reason.is_normal() {
                    // Kill the linked process
                    entry.sender.kill(ExitReason::Linked {
                        pid,
                        reason: Box::new(reason.clone()),
                    });
                }
            }
        }
    }

    // Notify monitoring processes
    if let Some(refs) = table.monitored_by.get(&pid) {
        for &monitor_ref in refs {
            if let Some((monitoring_pid, _)) = table.monitors.get(&monitor_ref) {
                if let Some(entry) = table.processes.get(monitoring_pid) {
                    entry.sender.send_down(pid, monitor_ref, reason.clone());
                }
            }
        }
    }
}

/// Create a bidirectional link between two processes.
///
/// If either process exits abnormally, the other will be notified.
pub fn link(pid_a: Pid, pid_b: Pid) -> Result<(), LinkError> {
    if pid_a == pid_b {
        return Err(LinkError::SelfLink);
    }

    let mut table = PROCESS_TABLE.write().unwrap();

    // Verify both processes exist
    if !table.processes.contains_key(&pid_a) {
        return Err(LinkError::ProcessNotFound(pid_a));
    }
    if !table.processes.contains_key(&pid_b) {
        return Err(LinkError::ProcessNotFound(pid_b));
    }

    // Create bidirectional link
    table.links.entry(pid_a).or_default().insert(pid_b);
    table.links.entry(pid_b).or_default().insert(pid_a);

    Ok(())
}

/// Remove a bidirectional link between two processes.
pub fn unlink(pid_a: Pid, pid_b: Pid) {
    let mut table = PROCESS_TABLE.write().unwrap();

    if let Some(links) = table.links.get_mut(&pid_a) {
        links.remove(&pid_b);
    }
    if let Some(links) = table.links.get_mut(&pid_b) {
        links.remove(&pid_a);
    }
}

/// Monitor a process.
///
/// Returns a MonitorRef that can be used to cancel the monitor.
/// When the monitored process exits, the monitoring process receives a DOWN message.
pub fn monitor(monitoring_pid: Pid, monitored_pid: Pid) -> Result<MonitorRef, LinkError> {
    let mut table = PROCESS_TABLE.write().unwrap();

    // Verify monitoring process exists
    if !table.processes.contains_key(&monitoring_pid) {
        return Err(LinkError::ProcessNotFound(monitoring_pid));
    }

    // If monitored process doesn't exist, immediately send DOWN
    if !table.processes.contains_key(&monitored_pid) {
        let monitor_ref = MonitorRef::new();
        if let Some(entry) = table.processes.get(&monitoring_pid) {
            entry
                .sender
                .send_down(monitored_pid, monitor_ref, ExitReason::Normal);
        }
        return Ok(monitor_ref);
    }

    let monitor_ref = MonitorRef::new();

    table
        .monitors
        .insert(monitor_ref, (monitoring_pid, monitored_pid));
    table
        .monitored_by
        .entry(monitored_pid)
        .or_default()
        .insert(monitor_ref);

    Ok(monitor_ref)
}

/// Stop monitoring a process.
pub fn demonitor(monitor_ref: MonitorRef) {
    let mut table = PROCESS_TABLE.write().unwrap();

    if let Some((_, monitored_pid)) = table.monitors.remove(&monitor_ref) {
        if let Some(refs) = table.monitored_by.get_mut(&monitored_pid) {
            refs.remove(&monitor_ref);
        }
    }
}

/// Set whether a process traps exits.
///
/// When trap_exit is true, EXIT messages from linked processes are delivered
/// via handle_info instead of causing the process to crash.
pub fn set_trap_exit(pid: Pid, trap: bool) {
    let mut table = PROCESS_TABLE.write().unwrap();
    if let Some(entry) = table.processes.get_mut(&pid) {
        entry.trap_exit = trap;
    }
}

/// Check if a process is trapping exits.
pub fn is_trapping_exit(pid: Pid) -> bool {
    let table = PROCESS_TABLE.read().unwrap();
    table
        .processes
        .get(&pid)
        .map(|e| e.trap_exit)
        .unwrap_or(false)
}

/// Check if a process is alive (registered in the table).
pub fn is_alive(pid: Pid) -> bool {
    let table = PROCESS_TABLE.read().unwrap();
    table
        .processes
        .get(&pid)
        .map(|e| e.sender.is_alive())
        .unwrap_or(false)
}

/// Get all processes linked to a given process.
pub fn get_links(pid: Pid) -> Vec<Pid> {
    let table = PROCESS_TABLE.read().unwrap();
    table
        .links
        .get(&pid)
        .map(|links| links.iter().copied().collect())
        .unwrap_or_default()
}

/// Get all monitor refs for monitors where pid is being monitored.
pub fn get_monitors(pid: Pid) -> Vec<MonitorRef> {
    let table = PROCESS_TABLE.read().unwrap();
    table
        .monitored_by
        .get(&pid)
        .map(|refs| refs.iter().copied().collect())
        .unwrap_or_default()
}

/// Information about a process.
///
/// This is the Rust equivalent of Erlang's `process_info/1`.
/// It provides a snapshot of a process's state for debugging and introspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    /// The process ID.
    pub pid: Pid,

    /// Whether the process is currently alive.
    pub alive: bool,

    /// Whether the process is trapping exits.
    pub trap_exit: bool,

    /// Processes linked to this one.
    pub links: Vec<Pid>,

    /// Monitor refs for processes monitoring this one.
    pub monitored_by: Vec<MonitorRef>,

    /// Monitor refs for processes this one is monitoring.
    pub monitoring: Vec<MonitorRef>,

    /// The registered name of this process (if any).
    pub registered_name: Option<String>,
}

/// Get information about a process.
///
/// Returns `None` if the process doesn't exist.
///
/// This is the equivalent of Erlang's `process_info/1`.
///
/// # Example
///
/// ```ignore
/// if let Some(info) = process_table::process_info(pid) {
///     println!("Process {} is alive: {}", info.pid, info.alive);
///     println!("Trap exit: {}", info.trap_exit);
///     println!("Links: {:?}", info.links);
///     println!("Monitored by: {:?}", info.monitored_by);
/// }
/// ```
pub fn process_info(pid: Pid) -> Option<ProcessInfo> {
    let table = PROCESS_TABLE.read().unwrap();

    let entry = table.processes.get(&pid)?;

    let links = table
        .links
        .get(&pid)
        .map(|l| l.iter().copied().collect())
        .unwrap_or_default();

    let monitored_by = table
        .monitored_by
        .get(&pid)
        .map(|refs| refs.iter().copied().collect())
        .unwrap_or_default();

    // Find monitors where this pid is the monitoring process
    let monitoring: Vec<MonitorRef> = table
        .monitors
        .iter()
        .filter(|(_, (monitoring_pid, _))| *monitoring_pid == pid)
        .map(|(ref_, _)| *ref_)
        .collect();

    // Get registered name from registry
    let registered_name = registry::name_of(pid);

    Some(ProcessInfo {
        pid,
        alive: entry.sender.is_alive(),
        trap_exit: entry.trap_exit,
        links,
        monitored_by,
        monitoring,
        registered_name,
    })
}

/// Get a list of all registered process PIDs.
///
/// This is useful for debugging and introspection.
pub fn all_processes() -> Vec<Pid> {
    let table = PROCESS_TABLE.read().unwrap();
    table.processes.keys().copied().collect()
}

/// Get the count of registered processes.
pub fn process_count() -> usize {
    let table = PROCESS_TABLE.read().unwrap();
    table.processes.len()
}

/// Send an exit signal to a process.
///
/// This is the equivalent of Erlang's `exit(Pid, Reason)`.
///
/// The behavior depends on the reason and whether the target process is trapping exits:
///
/// - If `reason` is `Kill`: The process is unconditionally terminated, even if it's
///   trapping exits. This is the "untrappable" kill signal.
///
/// - If `reason` is `Normal` and the target is the sender: The process exits normally.
///
/// - If `reason` is `Normal` and the target is NOT the sender: The signal is ignored
///   (a process cannot force another to exit "normally").
///
/// - For any other reason:
///   - If the target is trapping exits: It receives a `SystemMessage::Exit` with
///     the sender's pid and the reason.
///   - If the target is NOT trapping exits: The process is killed with the given reason.
///
/// # Arguments
///
/// * `from` - The pid of the process sending the exit signal
/// * `to` - The pid of the target process
/// * `reason` - The exit reason
///
/// # Returns
///
/// * `Ok(())` - The signal was sent (or ignored for Normal)
/// * `Err(LinkError::ProcessNotFound)` - The target process doesn't exist
///
/// # Example
///
/// ```ignore
/// // Kill a process unconditionally
/// process_table::exit(my_pid, target_pid, ExitReason::Kill)?;
///
/// // Send a custom exit reason (will be trapped if target is trapping)
/// process_table::exit(my_pid, target_pid, ExitReason::error("shutdown requested"))?;
/// ```
pub fn exit(from: Pid, to: Pid, reason: ExitReason) -> Result<(), LinkError> {
    let table = PROCESS_TABLE.read().unwrap();

    // Check if target exists
    let entry = table
        .processes
        .get(&to)
        .ok_or(LinkError::ProcessNotFound(to))?;

    match &reason {
        // Kill is untrappable - always kills the process
        ExitReason::Kill => {
            entry.sender.kill(reason);
        }

        // Normal exit signal to self - exit normally
        ExitReason::Normal if from == to => {
            entry.sender.kill(ExitReason::Normal);
        }

        // Normal exit signal to another process - ignored
        // (you can't force another process to exit "normally")
        ExitReason::Normal => {
            // Do nothing - this is the expected Erlang behavior
        }

        // Other reasons depend on trap_exit flag
        _ => {
            if entry.trap_exit {
                // Process is trapping exits - send as message
                entry.sender.send_exit(from, reason);
            } else {
                // Process is not trapping - kill it
                entry.sender.kill(reason);
            }
        }
    }

    Ok(())
}

/// Send an exit signal to a process without specifying a sender.
///
/// This is a convenience wrapper around [`exit`] for when you don't have
/// a sender pid (e.g., external shutdown requests).
///
/// The `from` pid is set to the same as `to`, which means:
/// - `Normal` will cause the process to exit normally
/// - Other reasons behave the same as regular `exit`
///
/// # Example
///
/// ```ignore
/// // Request a process to shut down
/// process_table::exit_self(target_pid, ExitReason::Shutdown)?;
/// ```
pub fn exit_self(pid: Pid, reason: ExitReason) -> Result<(), LinkError> {
    exit(pid, pid, reason)
}

/// Error type for link operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// Cannot link a process to itself.
    SelfLink,
    /// The specified process was not found.
    ProcessNotFound(Pid),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::SelfLink => write!(f, "cannot link a process to itself"),
            LinkError::ProcessNotFound(pid) => write!(f, "process {} not found", pid),
        }
    }
}

impl std::error::Error for LinkError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Mock sender for testing
    struct MockSender {
        alive: AtomicBool,
        down_received: Arc<RwLock<Vec<(Pid, MonitorRef, ExitReason)>>>,
        exit_received: Arc<RwLock<Vec<(Pid, ExitReason)>>>,
        kill_received: Arc<RwLock<Vec<ExitReason>>>,
    }

    impl MockSender {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                alive: AtomicBool::new(true),
                down_received: Arc::new(RwLock::new(Vec::new())),
                exit_received: Arc::new(RwLock::new(Vec::new())),
                kill_received: Arc::new(RwLock::new(Vec::new())),
            })
        }
    }

    impl SystemMessageSender for MockSender {
        fn send_down(&self, pid: Pid, monitor_ref: MonitorRef, reason: ExitReason) {
            self.down_received
                .write()
                .unwrap()
                .push((pid, monitor_ref, reason));
        }

        fn send_exit(&self, pid: Pid, reason: ExitReason) {
            self.exit_received.write().unwrap().push((pid, reason));
        }

        fn kill(&self, reason: ExitReason) {
            self.kill_received.write().unwrap().push(reason);
            self.alive.store(false, Ordering::SeqCst);
        }

        fn is_alive(&self) -> bool {
            self.alive.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn test_register_and_unregister() {
        let pid = Pid::new();
        let sender = MockSender::new();

        register(pid, sender);
        assert!(is_alive(pid));

        unregister(pid, ExitReason::Normal);
        assert!(!is_alive(pid));
    }

    #[test]
    fn test_link_self_error() {
        let pid = Pid::new();
        let sender = MockSender::new();
        register(pid, sender);

        let result = link(pid, pid);
        assert_eq!(result, Err(LinkError::SelfLink));

        unregister(pid, ExitReason::Normal);
    }

    #[test]
    fn test_link_not_found_error() {
        let pid1 = Pid::new();
        let pid2 = Pid::new(); // Not registered
        let sender = MockSender::new();
        register(pid1, sender);

        let result = link(pid1, pid2);
        assert_eq!(result, Err(LinkError::ProcessNotFound(pid2)));

        unregister(pid1, ExitReason::Normal);
    }

    #[test]
    fn test_link_and_unlink() {
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let sender1 = MockSender::new();
        let sender2 = MockSender::new();

        register(pid1, sender1);
        register(pid2, sender2);

        // Link
        assert!(link(pid1, pid2).is_ok());
        assert!(get_links(pid1).contains(&pid2));
        assert!(get_links(pid2).contains(&pid1));

        // Unlink
        unlink(pid1, pid2);
        assert!(!get_links(pid1).contains(&pid2));
        assert!(!get_links(pid2).contains(&pid1));

        unregister(pid1, ExitReason::Normal);
        unregister(pid2, ExitReason::Normal);
    }

    #[test]
    fn test_monitor_and_demonitor() {
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let sender1 = MockSender::new();
        let sender2 = MockSender::new();

        register(pid1, sender1);
        register(pid2, sender2);

        // Monitor
        let monitor_ref = monitor(pid1, pid2).unwrap();
        assert!(get_monitors(pid2).contains(&monitor_ref));

        // Demonitor
        demonitor(monitor_ref);
        assert!(!get_monitors(pid2).contains(&monitor_ref));

        unregister(pid1, ExitReason::Normal);
        unregister(pid2, ExitReason::Normal);
    }

    #[test]
    fn test_trap_exit() {
        let pid = Pid::new();
        let sender = MockSender::new();
        register(pid, sender);

        assert!(!is_trapping_exit(pid));
        set_trap_exit(pid, true);
        assert!(is_trapping_exit(pid));
        set_trap_exit(pid, false);
        assert!(!is_trapping_exit(pid));

        unregister(pid, ExitReason::Normal);
    }

    #[test]
    fn test_monitor_dead_process() {
        let pid1 = Pid::new();
        let pid2 = Pid::new(); // Not registered (dead)
        let sender1 = MockSender::new();
        let sender1_clone = sender1.clone();

        register(pid1, sender1);

        // Monitor dead process should succeed and send immediate DOWN
        let monitor_ref = monitor(pid1, pid2).unwrap();
        let downs = sender1_clone.down_received.read().unwrap();
        assert_eq!(downs.len(), 1);
        assert_eq!(downs[0].0, pid2);
        assert_eq!(downs[0].1, monitor_ref);

        unregister(pid1, ExitReason::Normal);
    }

    #[test]
    fn test_exit_kill_is_untrappable() {
        let pid = Pid::new();
        let sender = MockSender::new();
        let sender_clone = sender.clone();

        register(pid, sender);
        set_trap_exit(pid, true); // Even with trap_exit, Kill should work

        // Send Kill signal
        let result = exit(pid, pid, ExitReason::Kill);
        assert!(result.is_ok());

        // Should have been killed, not received as message
        let kills = sender_clone.kill_received.read().unwrap();
        assert_eq!(kills.len(), 1);
        assert_eq!(kills[0], ExitReason::Kill);

        let exits = sender_clone.exit_received.read().unwrap();
        assert!(exits.is_empty());

        unregister(pid, ExitReason::Normal);
    }

    #[test]
    fn test_exit_normal_to_self() {
        let pid = Pid::new();
        let sender = MockSender::new();
        let sender_clone = sender.clone();

        register(pid, sender);

        // Send Normal exit to self
        let result = exit(pid, pid, ExitReason::Normal);
        assert!(result.is_ok());

        // Should exit normally
        let kills = sender_clone.kill_received.read().unwrap();
        assert_eq!(kills.len(), 1);
        assert_eq!(kills[0], ExitReason::Normal);

        unregister(pid, ExitReason::Normal);
    }

    #[test]
    fn test_exit_normal_to_other_is_ignored() {
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let sender1 = MockSender::new();
        let sender2 = MockSender::new();
        let sender2_clone = sender2.clone();

        register(pid1, sender1);
        register(pid2, sender2);

        // Send Normal exit from pid1 to pid2
        let result = exit(pid1, pid2, ExitReason::Normal);
        assert!(result.is_ok());

        // pid2 should NOT be affected (Normal from another process is ignored)
        let kills = sender2_clone.kill_received.read().unwrap();
        assert!(kills.is_empty());

        let exits = sender2_clone.exit_received.read().unwrap();
        assert!(exits.is_empty());

        assert!(sender2_clone.is_alive());

        unregister(pid1, ExitReason::Normal);
        unregister(pid2, ExitReason::Normal);
    }

    #[test]
    fn test_exit_error_kills_non_trapping_process() {
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let sender1 = MockSender::new();
        let sender2 = MockSender::new();
        let sender2_clone = sender2.clone();

        register(pid1, sender1);
        register(pid2, sender2);
        // pid2 is NOT trapping exits (default)

        // Send error exit from pid1 to pid2
        let result = exit(pid1, pid2, ExitReason::error("test error"));
        assert!(result.is_ok());

        // pid2 should be killed
        let kills = sender2_clone.kill_received.read().unwrap();
        assert_eq!(kills.len(), 1);
        assert_eq!(kills[0], ExitReason::error("test error"));

        unregister(pid1, ExitReason::Normal);
        unregister(pid2, ExitReason::Normal);
    }

    #[test]
    fn test_exit_error_is_trapped_when_trapping() {
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let sender1 = MockSender::new();
        let sender2 = MockSender::new();
        let sender2_clone = sender2.clone();

        register(pid1, sender1);
        register(pid2, sender2);
        set_trap_exit(pid2, true); // pid2 IS trapping exits

        // Send error exit from pid1 to pid2
        let result = exit(pid1, pid2, ExitReason::error("test error"));
        assert!(result.is_ok());

        // pid2 should receive it as a message, not be killed
        let kills = sender2_clone.kill_received.read().unwrap();
        assert!(kills.is_empty());

        let exits = sender2_clone.exit_received.read().unwrap();
        assert_eq!(exits.len(), 1);
        assert_eq!(exits[0].0, pid1); // from pid
        assert_eq!(exits[0].1, ExitReason::error("test error"));

        assert!(sender2_clone.is_alive());

        unregister(pid1, ExitReason::Normal);
        unregister(pid2, ExitReason::Normal);
    }

    #[test]
    fn test_exit_shutdown_behavior() {
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let sender1 = MockSender::new();
        let sender2 = MockSender::new();
        let sender2_clone = sender2.clone();

        register(pid1, sender1);
        register(pid2, sender2);
        // pid2 is NOT trapping exits

        // Send Shutdown exit from pid1 to pid2
        let result = exit(pid1, pid2, ExitReason::Shutdown);
        assert!(result.is_ok());

        // pid2 should be killed (Shutdown is not special like Normal)
        let kills = sender2_clone.kill_received.read().unwrap();
        assert_eq!(kills.len(), 1);
        assert_eq!(kills[0], ExitReason::Shutdown);

        unregister(pid1, ExitReason::Normal);
        unregister(pid2, ExitReason::Normal);
    }

    #[test]
    fn test_exit_to_nonexistent_process() {
        let pid1 = Pid::new();
        let pid2 = Pid::new(); // Not registered
        let sender1 = MockSender::new();

        register(pid1, sender1);

        let result = exit(pid1, pid2, ExitReason::Kill);
        assert_eq!(result, Err(LinkError::ProcessNotFound(pid2)));

        unregister(pid1, ExitReason::Normal);
    }

    #[test]
    fn test_exit_self_convenience() {
        let pid = Pid::new();
        let sender = MockSender::new();
        let sender_clone = sender.clone();

        register(pid, sender);

        // Use exit_self convenience function
        let result = exit_self(pid, ExitReason::Shutdown);
        assert!(result.is_ok());

        let kills = sender_clone.kill_received.read().unwrap();
        assert_eq!(kills.len(), 1);
        assert_eq!(kills[0], ExitReason::Shutdown);

        unregister(pid, ExitReason::Normal);
    }

    #[test]
    fn test_process_info_basic() {
        let pid = Pid::new();
        let sender = MockSender::new();

        register(pid, sender);

        let info = process_info(pid).expect("process should exist");
        assert_eq!(info.pid, pid);
        assert!(info.alive);
        assert!(!info.trap_exit);
        assert!(info.links.is_empty());
        assert!(info.monitored_by.is_empty());
        assert!(info.monitoring.is_empty());
        assert!(info.registered_name.is_none());

        unregister(pid, ExitReason::Normal);
    }

    #[test]
    fn test_process_info_nonexistent() {
        let pid = Pid::new(); // Not registered
        assert!(process_info(pid).is_none());
    }

    #[test]
    fn test_process_info_with_trap_exit() {
        let pid = Pid::new();
        let sender = MockSender::new();

        register(pid, sender);
        set_trap_exit(pid, true);

        let info = process_info(pid).expect("process should exist");
        assert!(info.trap_exit);

        unregister(pid, ExitReason::Normal);
    }

    #[test]
    fn test_process_info_with_links() {
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let sender1 = MockSender::new();
        let sender2 = MockSender::new();

        register(pid1, sender1);
        register(pid2, sender2);
        link(pid1, pid2).unwrap();

        let info1 = process_info(pid1).expect("process should exist");
        assert!(info1.links.contains(&pid2));

        let info2 = process_info(pid2).expect("process should exist");
        assert!(info2.links.contains(&pid1));

        unregister(pid1, ExitReason::Normal);
        unregister(pid2, ExitReason::Normal);
    }

    #[test]
    fn test_process_info_with_monitors() {
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let sender1 = MockSender::new();
        let sender2 = MockSender::new();

        register(pid1, sender1);
        register(pid2, sender2);

        // pid1 monitors pid2
        let monitor_ref = monitor(pid1, pid2).unwrap();

        // pid2 should show it's being monitored
        let info2 = process_info(pid2).expect("process should exist");
        assert!(info2.monitored_by.contains(&monitor_ref));

        // pid1 should show it's monitoring
        let info1 = process_info(pid1).expect("process should exist");
        assert!(info1.monitoring.contains(&monitor_ref));

        unregister(pid1, ExitReason::Normal);
        unregister(pid2, ExitReason::Normal);
    }

    #[test]
    fn test_all_processes() {
        // Note: Due to test parallelism, we can't rely on absolute counts.
        // Instead, we verify that our processes appear in the list.
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let sender1 = MockSender::new();
        let sender2 = MockSender::new();

        register(pid1, sender1);
        register(pid2, sender2);

        let all = all_processes();
        assert!(all.contains(&pid1), "pid1 should be in all_processes");
        assert!(all.contains(&pid2), "pid2 should be in all_processes");
        assert!(all.len() >= 2, "Should have at least our 2 processes");

        unregister(pid1, ExitReason::Normal);
        unregister(pid2, ExitReason::Normal);

        // After unregister, our pids should be gone
        let all_after = all_processes();
        assert!(!all_after.contains(&pid1), "pid1 should be gone");
        assert!(!all_after.contains(&pid2), "pid2 should be gone");
    }

    #[test]
    fn test_process_count() {
        // Note: Due to test parallelism, we can't rely on absolute counts.
        // We verify that our specific processes are tracked correctly.
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let pid3 = Pid::new();
        let sender1 = MockSender::new();
        let sender2 = MockSender::new();
        let sender3 = MockSender::new();

        register(pid1, sender1);
        register(pid2, sender2);
        register(pid3, sender3);

        // Verify our processes are in the list
        let all = all_processes();
        assert!(all.contains(&pid1), "pid1 should be registered");
        assert!(all.contains(&pid2), "pid2 should be registered");
        assert!(all.contains(&pid3), "pid3 should be registered");

        // Count should be at least 3 (our processes)
        assert!(process_count() >= 3, "Should have at least our 3 processes");

        unregister(pid1, ExitReason::Normal);
        unregister(pid2, ExitReason::Normal);
        unregister(pid3, ExitReason::Normal);

        // After unregister, our pids should be gone
        let all_after = all_processes();
        assert!(!all_after.contains(&pid1), "pid1 should be unregistered");
        assert!(!all_after.contains(&pid2), "pid2 should be unregistered");
        assert!(!all_after.contains(&pid3), "pid3 should be unregistered");
    }
}
