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
/// SystemMessage to a Actor's mailbox.
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
struct ActorEntry {
    /// Sender for system messages.
    sender: Arc<dyn SystemMessageSender>,
    /// Whether this process traps exits.
    trap_exit: bool,
}

/// Global process table.
///
/// This is a singleton that tracks all active processes, their links, and monitors.
struct ActorTableInner {
    /// All registered processes.
    processes: HashMap<Pid, ActorEntry>,

    /// Bidirectional links: pid -> set of linked pids.
    links: HashMap<Pid, HashSet<Pid>>,

    /// Active monitors: monitor_ref -> (monitoring_pid, monitored_pid).
    monitors: HashMap<MonitorRef, (Pid, Pid)>,

    /// Reverse lookup: pid -> set of monitor refs watching this pid.
    monitored_by: HashMap<Pid, HashSet<MonitorRef>>,
}

impl ActorTableInner {
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
static ACTOR_TABLE: std::sync::LazyLock<RwLock<ActorTableInner>> =
    std::sync::LazyLock::new(|| RwLock::new(ActorTableInner::new()));

/// Register a process with the table.
///
/// Called when a Actor starts.
pub fn register(pid: Pid, sender: Arc<dyn SystemMessageSender>) {
    let mut table = ACTOR_TABLE.write().unwrap();
    table.processes.insert(
        pid,
        ActorEntry {
            sender,
            trap_exit: false,
        },
    );
}

/// Unregister a process from the table.
///
/// Called when a Actor terminates. Also cleans up links, monitors, and registry.
pub fn unregister(pid: Pid, reason: ExitReason) {
    // First, notify linked and monitoring processes
    notify_exit(pid, reason);

    // Clean up the registry (remove any registered name for this pid)
    registry::unregister_pid(pid);

    // Then clean up the table
    let mut table = ACTOR_TABLE.write().unwrap();

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
    let table = ACTOR_TABLE.read().unwrap();

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

    let mut table = ACTOR_TABLE.write().unwrap();

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
    let mut table = ACTOR_TABLE.write().unwrap();

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
    let mut table = ACTOR_TABLE.write().unwrap();

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
    let mut table = ACTOR_TABLE.write().unwrap();

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
    let mut table = ACTOR_TABLE.write().unwrap();
    if let Some(entry) = table.processes.get_mut(&pid) {
        entry.trap_exit = trap;
    }
}

/// Check if a process is trapping exits.
pub fn is_trapping_exit(pid: Pid) -> bool {
    let table = ACTOR_TABLE.read().unwrap();
    table
        .processes
        .get(&pid)
        .map(|e| e.trap_exit)
        .unwrap_or(false)
}

/// Check if a process is alive (registered in the table).
pub fn is_alive(pid: Pid) -> bool {
    let table = ACTOR_TABLE.read().unwrap();
    table
        .processes
        .get(&pid)
        .map(|e| e.sender.is_alive())
        .unwrap_or(false)
}

/// Get all processes linked to a given process.
pub fn get_links(pid: Pid) -> Vec<Pid> {
    let table = ACTOR_TABLE.read().unwrap();
    table
        .links
        .get(&pid)
        .map(|links| links.iter().copied().collect())
        .unwrap_or_default()
}

/// Get all monitor refs for monitors where pid is being monitored.
pub fn get_monitors(pid: Pid) -> Vec<MonitorRef> {
    let table = ACTOR_TABLE.read().unwrap();
    table
        .monitored_by
        .get(&pid)
        .map(|refs| refs.iter().copied().collect())
        .unwrap_or_default()
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
    fn test_linked_process_killed_on_abnormal_exit() {
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let sender1 = MockSender::new();
        let sender2 = MockSender::new();
        let sender2_clone = sender2.clone();

        register(pid1, sender1);
        register(pid2, sender2);

        // Link the two processes
        assert!(link(pid1, pid2).is_ok());

        // pid1 exits abnormally - pid2 should be killed
        unregister(pid1, ExitReason::Error("crashed".to_string()));

        // Verify pid2 received a kill signal
        let kills = sender2_clone.kill_received.read().unwrap();
        assert_eq!(kills.len(), 1, "Linked process should be killed on abnormal exit");
        match &kills[0] {
            ExitReason::Linked { pid, .. } => assert_eq!(*pid, pid1),
            other => panic!("Expected Linked exit reason, got {:?}", other),
        }

        // Clean up
        unregister(pid2, ExitReason::Normal);
    }

    #[test]
    fn test_linked_process_survives_normal_exit() {
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let sender1 = MockSender::new();
        let sender2 = MockSender::new();
        let sender2_clone = sender2.clone();

        register(pid1, sender1);
        register(pid2, sender2);

        // Link the two processes
        assert!(link(pid1, pid2).is_ok());

        // pid1 exits normally - pid2 should NOT be killed
        unregister(pid1, ExitReason::Normal);

        // Verify pid2 did NOT receive a kill signal
        let kills = sender2_clone.kill_received.read().unwrap();
        assert_eq!(kills.len(), 0, "Linked process should NOT be killed on normal exit");

        // pid2 should still be alive
        assert!(is_alive(pid2));

        // Clean up
        unregister(pid2, ExitReason::Normal);
    }

    #[test]
    fn test_trap_exit_receives_message_instead_of_kill() {
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let sender1 = MockSender::new();
        let sender2 = MockSender::new();
        let sender2_clone = sender2.clone();

        register(pid1, sender1);
        register(pid2, sender2);

        // Link the two processes
        assert!(link(pid1, pid2).is_ok());

        // pid2 traps exits
        set_trap_exit(pid2, true);

        // pid1 exits abnormally - pid2 should receive EXIT message, not be killed
        unregister(pid1, ExitReason::Error("crashed".to_string()));

        // Verify pid2 received an EXIT message (not killed)
        let kills = sender2_clone.kill_received.read().unwrap();
        assert_eq!(kills.len(), 0, "Process trapping exits should NOT be killed");

        let exits = sender2_clone.exit_received.read().unwrap();
        assert_eq!(exits.len(), 1, "Process trapping exits should receive EXIT message");
        assert_eq!(exits[0].0, pid1, "EXIT message should be from the crashed process");
        match &exits[0].1 {
            ExitReason::Error(msg) => assert_eq!(msg, "crashed"),
            other => panic!("Expected Error exit reason, got {:?}", other),
        }

        // pid2 should still be alive
        assert!(is_alive(pid2));

        // Clean up
        unregister(pid2, ExitReason::Normal);
    }
}
