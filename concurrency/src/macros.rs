//! Macros for simplifying common patterns.
//!
//! This module provides convenience macros for:
//! - Creating child specifications
//! - Defining supervisor trees
//!
//! # Example
//!
//! ```ignore
//! use spawned_concurrency::{child_spec, supervisor_spec};
//!
//! let spec = supervisor_spec!(
//!     OneForOne,
//!     [
//!         child_spec!(worker "counter", Counter::new(0).start()),
//!         child_spec!(worker "logger", Logger::new().start()),
//!     ]
//! );
//! ```

/// Create a child specification with a simple syntax.
///
/// # Syntax
///
/// ```ignore
/// // Basic worker
/// child_spec!(worker "name", expression)
///
/// // Supervisor child
/// child_spec!(supervisor "name", expression)
/// ```
///
/// # Examples
///
/// ```ignore
/// // Simple worker
/// let spec = child_spec!(worker "counter", Counter::new(0).start());
///
/// // Supervisor child
/// let spec = child_spec!(supervisor "sub_sup", SubSupervisor::start(spec));
/// ```
#[macro_export]
macro_rules! child_spec {
    // Worker
    (worker $id:expr, $start:expr) => {
        $crate::supervisor::ChildSpec::worker($id, move || $start)
    };

    // Supervisor child
    (supervisor $id:expr, $start:expr) => {
        $crate::supervisor::ChildSpec::supervisor($id, move || $start)
    };
}

/// Create a supervisor specification with a declarative syntax.
///
/// # Syntax
///
/// ```ignore
/// // Basic
/// supervisor_spec!(Strategy, [children...])
///
/// // With max restarts
/// supervisor_spec!(Strategy, max_restarts(3, 5), [children...])
/// ```
///
/// # Examples
///
/// ```ignore
/// let spec = supervisor_spec!(
///     OneForOne,
///     [
///         child_spec!(worker "worker1", Worker::new().start()),
///         child_spec!(worker "worker2", Worker::new().start()),
///     ]
/// );
///
/// let spec = supervisor_spec!(
///     OneForAll,
///     max_restarts(5, 60),
///     [
///         child_spec!(worker "db", Db::new().start()),
///         child_spec!(worker "cache", Cache::new().start()),
///     ]
/// );
/// ```
#[macro_export]
macro_rules! supervisor_spec {
    // With max_restarts
    ($strategy:ident, max_restarts($count:expr, $secs:expr), [ $($child:expr),* $(,)? ]) => {{
        let mut spec = $crate::supervisor::SupervisorSpec::new(
            $crate::supervisor::RestartStrategy::$strategy
        ).max_restarts($count, ::std::time::Duration::from_secs($secs));
        $(
            spec = spec.child($child);
        )*
        spec
    }};

    // Without max_restarts
    ($strategy:ident, [ $($child:expr),* $(,)? ]) => {{
        let mut spec = $crate::supervisor::SupervisorSpec::new(
            $crate::supervisor::RestartStrategy::$strategy
        );
        $(
            spec = spec.child($child);
        )*
        spec
    }};
}

/// Create a dynamic supervisor specification.
///
/// Dynamic supervisors always use OneForOne strategy since children are
/// started dynamically and independently.
///
/// # Syntax
///
/// ```ignore
/// dynamic_supervisor_spec!()
/// dynamic_supervisor_spec!(max_children(100))
/// dynamic_supervisor_spec!(max_restarts(3, 5))
/// dynamic_supervisor_spec!(max_children(100), max_restarts(3, 5))
/// ```
///
/// # Examples
///
/// ```ignore
/// let spec = dynamic_supervisor_spec!();
///
/// let spec = dynamic_supervisor_spec!(max_children(50), max_restarts(10, 60));
/// ```
#[macro_export]
macro_rules! dynamic_supervisor_spec {
    // Empty - just defaults
    () => {
        $crate::supervisor::DynamicSupervisorSpec::new()
    };

    // Just max_children
    (max_children($max:expr)) => {
        $crate::supervisor::DynamicSupervisorSpec::new().max_children($max)
    };

    // Just max_restarts
    (max_restarts($count:expr, $secs:expr)) => {
        $crate::supervisor::DynamicSupervisorSpec::new()
            .max_restarts($count, ::std::time::Duration::from_secs($secs))
    };

    // max_children and max_restarts
    (max_children($max:expr), max_restarts($count:expr, $secs:expr)) => {
        $crate::supervisor::DynamicSupervisorSpec::new()
            .max_children($max)
            .max_restarts($count, ::std::time::Duration::from_secs($secs))
    };
}

#[cfg(test)]
mod tests {
    use crate::supervisor::RestartStrategy;
    use crate::tasks::GenServer;

    // Mock GenServer for testing
    struct MockServer;

    impl crate::tasks::GenServer for MockServer {
        type CallMsg = ();
        type CastMsg = ();
        type OutMsg = ();
        type Error = ();

        async fn handle_call(
            &mut self,
            _: Self::CallMsg,
            _: &crate::tasks::GenServerHandle<Self>,
        ) -> crate::tasks::CallResponse<Self> {
            crate::tasks::CallResponse::Reply(())
        }

        async fn handle_cast(
            &mut self,
            _: Self::CastMsg,
            _: &crate::tasks::GenServerHandle<Self>,
        ) -> crate::tasks::CastResponse {
            crate::tasks::CastResponse::NoReply
        }
    }

    #[test]
    fn test_child_spec_worker_macro() {
        let spec = child_spec!(worker "test", MockServer.start());
        assert_eq!(spec.id(), "test");
    }

    #[test]
    fn test_child_spec_supervisor_macro() {
        let spec = child_spec!(supervisor "test_sup", MockServer.start());
        assert_eq!(spec.id(), "test_sup");
    }

    #[test]
    fn test_supervisor_spec_macro() {
        let spec = supervisor_spec!(
            OneForOne,
            [child_spec!(worker "w1", MockServer.start())]
        );
        assert_eq!(spec.strategy(), RestartStrategy::OneForOne);
    }

    #[test]
    fn test_supervisor_spec_with_max_restarts() {
        let spec = supervisor_spec!(
            OneForAll,
            max_restarts(5, 60),
            [child_spec!(worker "w1", MockServer.start())]
        );
        assert_eq!(spec.strategy(), RestartStrategy::OneForAll);
    }

    #[test]
    fn test_supervisor_spec_multiple_children() {
        let spec = supervisor_spec!(
            RestForOne,
            [
                child_spec!(worker "w1", MockServer.start()),
                child_spec!(worker "w2", MockServer.start()),
                child_spec!(supervisor "sub", MockServer.start()),
            ]
        );
        assert_eq!(spec.strategy(), RestartStrategy::RestForOne);
    }

    #[test]
    fn test_dynamic_supervisor_spec_macro() {
        let _spec = dynamic_supervisor_spec!();
        // Just verify it compiles
    }

    #[test]
    fn test_dynamic_supervisor_spec_with_max_children() {
        let spec = dynamic_supervisor_spec!(max_children(50));
        assert_eq!(spec.get_max_children(), Some(50));
    }

    #[test]
    fn test_dynamic_supervisor_spec_with_max_restarts() {
        let _spec = dynamic_supervisor_spec!(max_restarts(10, 60));
        // Just verify it compiles
    }

    #[test]
    fn test_dynamic_supervisor_spec_with_both_options() {
        let spec = dynamic_supervisor_spec!(max_children(100), max_restarts(5, 30));
        assert_eq!(spec.get_max_children(), Some(100));
    }
}
