//! Process groups for pub/sub patterns.
//!
//! This module provides a way to group processes together for message broadcasting
//! and discovery. It's the Rust equivalent of Erlang's `pg` module.
//!
//! # Example
//!
//! ```ignore
//! use spawned_concurrency::pg;
//!
//! // Join a group
//! pg::join("workers", pid)?;
//!
//! // Get all members of a group
//! let members = pg::get_members("workers");
//!
//! // Leave a group
//! pg::leave("workers", pid);
//!
//! // Get all groups a process belongs to
//! let groups = pg::which_groups(pid);
//! ```

use crate::pid::Pid;
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

/// Internal state for the process group registry.
struct PgInner {
    /// Group name -> set of member PIDs.
    groups: HashMap<String, HashSet<Pid>>,

    /// Pid -> set of group names (reverse lookup for cleanup).
    pid_to_groups: HashMap<Pid, HashSet<String>>,
}

impl PgInner {
    fn new() -> Self {
        Self {
            groups: HashMap::new(),
            pid_to_groups: HashMap::new(),
        }
    }
}

/// Global process group registry.
static PG_REGISTRY: std::sync::LazyLock<RwLock<PgInner>> =
    std::sync::LazyLock::new(|| RwLock::new(PgInner::new()));

/// Error type for process group operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgError {
    /// The process is already a member of this group.
    AlreadyMember,
    /// The process is not a member of this group.
    NotMember,
}

impl std::fmt::Display for PgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PgError::AlreadyMember => write!(f, "process is already a member of this group"),
            PgError::NotMember => write!(f, "process is not a member of this group"),
        }
    }
}

impl std::error::Error for PgError {}

/// Join a process group.
///
/// Adds the process to the named group. A process can be a member of
/// multiple groups simultaneously.
///
/// # Arguments
///
/// * `group` - The name of the group to join
/// * `pid` - The process ID to add to the group
///
/// # Returns
///
/// * `Ok(())` - Successfully joined the group
/// * `Err(PgError::AlreadyMember)` - Process is already a member
///
/// # Example
///
/// ```ignore
/// pg::join("workers", handle.pid())?;
/// pg::join("logging", handle.pid())?;
/// ```
pub fn join(group: impl Into<String>, pid: Pid) -> Result<(), PgError> {
    let group = group.into();
    let mut registry = PG_REGISTRY.write().unwrap();

    // Check if already a member
    if let Some(members) = registry.groups.get(&group) {
        if members.contains(&pid) {
            return Err(PgError::AlreadyMember);
        }
    }

    // Add to group
    registry.groups.entry(group.clone()).or_default().insert(pid);

    // Add reverse mapping
    registry.pid_to_groups.entry(pid).or_default().insert(group);

    Ok(())
}

/// Join a process group, ignoring if already a member.
///
/// This is a convenience function that doesn't return an error if the
/// process is already in the group.
///
/// # Example
///
/// ```ignore
/// pg::join_or_ignore("workers", handle.pid());
/// ```
pub fn join_or_ignore(group: impl Into<String>, pid: Pid) {
    let _ = join(group, pid);
}

/// Leave a process group.
///
/// Removes the process from the named group.
///
/// # Arguments
///
/// * `group` - The name of the group to leave
/// * `pid` - The process ID to remove from the group
///
/// # Returns
///
/// * `Ok(())` - Successfully left the group
/// * `Err(PgError::NotMember)` - Process was not a member
///
/// # Example
///
/// ```ignore
/// pg::leave("workers", handle.pid())?;
/// ```
pub fn leave(group: &str, pid: Pid) -> Result<(), PgError> {
    let mut registry = PG_REGISTRY.write().unwrap();

    // Remove from group
    let was_member = if let Some(members) = registry.groups.get_mut(group) {
        members.remove(&pid)
    } else {
        false
    };

    if !was_member {
        return Err(PgError::NotMember);
    }

    // Clean up empty groups
    if let Some(members) = registry.groups.get(group) {
        if members.is_empty() {
            registry.groups.remove(group);
        }
    }

    // Remove reverse mapping
    if let Some(groups) = registry.pid_to_groups.get_mut(&pid) {
        groups.remove(group);
        if groups.is_empty() {
            registry.pid_to_groups.remove(&pid);
        }
    }

    Ok(())
}

/// Leave a process group, ignoring if not a member.
///
/// This is a convenience function that doesn't return an error if the
/// process is not in the group.
pub fn leave_or_ignore(group: &str, pid: Pid) {
    let _ = leave(group, pid);
}

/// Leave all groups that a process belongs to.
///
/// This should be called when a process terminates to clean up its
/// group memberships.
///
/// # Arguments
///
/// * `pid` - The process ID to remove from all groups
///
/// # Example
///
/// ```ignore
/// // In teardown or when process exits:
/// pg::leave_all(handle.pid());
/// ```
pub fn leave_all(pid: Pid) {
    let mut registry = PG_REGISTRY.write().unwrap();

    // Get all groups this pid belongs to
    let groups_to_leave: Vec<String> = registry
        .pid_to_groups
        .remove(&pid)
        .map(|groups| groups.into_iter().collect())
        .unwrap_or_default();

    // Remove from each group
    for group in groups_to_leave {
        if let Some(members) = registry.groups.get_mut(&group) {
            members.remove(&pid);
            if members.is_empty() {
                registry.groups.remove(&group);
            }
        }
    }
}

/// Get all members of a process group.
///
/// Returns an empty vector if the group doesn't exist.
///
/// # Arguments
///
/// * `group` - The name of the group
///
/// # Returns
///
/// A vector of PIDs that are members of the group.
///
/// # Example
///
/// ```ignore
/// let workers = pg::get_members("workers");
/// for pid in workers {
///     // Send message to each worker
/// }
/// ```
pub fn get_members(group: &str) -> Vec<Pid> {
    let registry = PG_REGISTRY.read().unwrap();
    registry
        .groups
        .get(group)
        .map(|members| members.iter().copied().collect())
        .unwrap_or_default()
}

/// Get all groups that a process belongs to.
///
/// Returns an empty vector if the process is not in any groups.
///
/// # Arguments
///
/// * `pid` - The process ID to query
///
/// # Returns
///
/// A vector of group names.
///
/// # Example
///
/// ```ignore
/// let groups = pg::which_groups(handle.pid());
/// println!("Process is in groups: {:?}", groups);
/// ```
pub fn which_groups(pid: Pid) -> Vec<String> {
    let registry = PG_REGISTRY.read().unwrap();
    registry
        .pid_to_groups
        .get(&pid)
        .map(|groups| groups.iter().cloned().collect())
        .unwrap_or_default()
}

/// Check if a process is a member of a group.
///
/// # Arguments
///
/// * `group` - The name of the group
/// * `pid` - The process ID to check
///
/// # Returns
///
/// `true` if the process is a member of the group.
pub fn is_member(group: &str, pid: Pid) -> bool {
    let registry = PG_REGISTRY.read().unwrap();
    registry
        .groups
        .get(group)
        .map(|members| members.contains(&pid))
        .unwrap_or(false)
}

/// Get the number of members in a group.
///
/// Returns 0 if the group doesn't exist.
pub fn member_count(group: &str) -> usize {
    let registry = PG_REGISTRY.read().unwrap();
    registry
        .groups
        .get(group)
        .map(|members| members.len())
        .unwrap_or(0)
}

/// Get all existing group names.
///
/// # Returns
///
/// A vector of all group names that have at least one member.
pub fn all_groups() -> Vec<String> {
    let registry = PG_REGISTRY.read().unwrap();
    registry.groups.keys().cloned().collect()
}

/// Clear all groups (for testing).
#[cfg(test)]
pub fn clear() {
    let mut registry = PG_REGISTRY.write().unwrap();
    registry.groups.clear();
    registry.pid_to_groups.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pid::Pid;

    #[test]
    fn test_join_and_get_members() {
        let pid1 = Pid::new();
        let pid2 = Pid::new();

        join("test_group_1", pid1).unwrap();
        join("test_group_1", pid2).unwrap();

        let members = get_members("test_group_1");
        assert!(members.contains(&pid1));
        assert!(members.contains(&pid2));
        assert_eq!(members.len(), 2);

        // Cleanup
        leave_all(pid1);
        leave_all(pid2);
    }

    #[test]
    fn test_join_already_member() {
        let pid = Pid::new();

        join("test_group_2", pid).unwrap();
        let result = join("test_group_2", pid);
        assert_eq!(result, Err(PgError::AlreadyMember));

        // Cleanup
        leave_all(pid);
    }

    #[test]
    fn test_join_or_ignore() {
        let pid = Pid::new();

        join_or_ignore("test_group_3", pid);
        join_or_ignore("test_group_3", pid); // Should not panic

        assert!(is_member("test_group_3", pid));

        // Cleanup
        leave_all(pid);
    }

    #[test]
    fn test_leave() {
        let pid = Pid::new();

        join("test_group_4", pid).unwrap();
        assert!(is_member("test_group_4", pid));

        leave("test_group_4", pid).unwrap();
        assert!(!is_member("test_group_4", pid));
    }

    #[test]
    fn test_leave_not_member() {
        let pid = Pid::new();
        let result = leave("nonexistent_group", pid);
        assert_eq!(result, Err(PgError::NotMember));
    }

    #[test]
    fn test_leave_or_ignore() {
        let pid = Pid::new();
        leave_or_ignore("nonexistent_group_2", pid); // Should not panic
    }

    #[test]
    fn test_leave_all() {
        let pid = Pid::new();

        join("group_a", pid).unwrap();
        join("group_b", pid).unwrap();
        join("group_c", pid).unwrap();

        assert_eq!(which_groups(pid).len(), 3);

        leave_all(pid);

        assert!(which_groups(pid).is_empty());
        assert!(!is_member("group_a", pid));
        assert!(!is_member("group_b", pid));
        assert!(!is_member("group_c", pid));
    }

    #[test]
    fn test_which_groups() {
        let pid = Pid::new();

        join("test_which_1", pid).unwrap();
        join("test_which_2", pid).unwrap();

        let groups = which_groups(pid);
        assert!(groups.contains(&"test_which_1".to_string()));
        assert!(groups.contains(&"test_which_2".to_string()));
        assert_eq!(groups.len(), 2);

        // Cleanup
        leave_all(pid);
    }

    #[test]
    fn test_is_member() {
        let pid = Pid::new();
        let other_pid = Pid::new();

        join("test_is_member", pid).unwrap();

        assert!(is_member("test_is_member", pid));
        assert!(!is_member("test_is_member", other_pid));
        assert!(!is_member("nonexistent", pid));

        // Cleanup
        leave_all(pid);
    }

    #[test]
    fn test_member_count() {
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let pid3 = Pid::new();

        assert_eq!(member_count("test_count"), 0);

        join("test_count", pid1).unwrap();
        assert_eq!(member_count("test_count"), 1);

        join("test_count", pid2).unwrap();
        assert_eq!(member_count("test_count"), 2);

        join("test_count", pid3).unwrap();
        assert_eq!(member_count("test_count"), 3);

        leave("test_count", pid1).unwrap();
        assert_eq!(member_count("test_count"), 2);

        // Cleanup
        leave_all(pid2);
        leave_all(pid3);
    }

    #[test]
    fn test_get_members_empty_group() {
        let members = get_members("nonexistent_group_99");
        assert!(members.is_empty());
    }

    #[test]
    fn test_all_groups() {
        let pid = Pid::new();

        join("all_groups_test_1", pid).unwrap();
        join("all_groups_test_2", pid).unwrap();

        let groups = all_groups();
        assert!(groups.contains(&"all_groups_test_1".to_string()));
        assert!(groups.contains(&"all_groups_test_2".to_string()));

        // Cleanup
        leave_all(pid);
    }

    #[test]
    fn test_empty_group_removed() {
        let pid = Pid::new();

        join("test_empty_removal", pid).unwrap();
        assert!(all_groups().contains(&"test_empty_removal".to_string()));

        leave("test_empty_removal", pid).unwrap();

        // Group should be removed when empty
        let registry = PG_REGISTRY.read().unwrap();
        assert!(!registry.groups.contains_key("test_empty_removal"));
    }

    #[test]
    fn test_multiple_processes_multiple_groups() {
        let pid1 = Pid::new();
        let pid2 = Pid::new();
        let pid3 = Pid::new();

        // pid1 in groups A and B
        join("multi_test_A", pid1).unwrap();
        join("multi_test_B", pid1).unwrap();

        // pid2 in groups B and C
        join("multi_test_B", pid2).unwrap();
        join("multi_test_C", pid2).unwrap();

        // pid3 only in group A
        join("multi_test_A", pid3).unwrap();

        // Check group A
        let group_a = get_members("multi_test_A");
        assert!(group_a.contains(&pid1));
        assert!(group_a.contains(&pid3));
        assert!(!group_a.contains(&pid2));

        // Check group B
        let group_b = get_members("multi_test_B");
        assert!(group_b.contains(&pid1));
        assert!(group_b.contains(&pid2));
        assert!(!group_b.contains(&pid3));

        // Check group C
        let group_c = get_members("multi_test_C");
        assert!(group_c.contains(&pid2));
        assert!(!group_c.contains(&pid1));
        assert!(!group_c.contains(&pid3));

        // Cleanup
        leave_all(pid1);
        leave_all(pid2);
        leave_all(pid3);
    }

    #[test]
    fn test_pg_error_display() {
        assert_eq!(
            PgError::AlreadyMember.to_string(),
            "process is already a member of this group"
        );
        assert_eq!(
            PgError::NotMember.to_string(),
            "process is not a member of this group"
        );
    }
}
