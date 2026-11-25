//! Listings and snapshots (introspection step 2; docs/design/introspection.md,
//! "Listing and snapshots"; semantics.md I6): the process-wide actor and
//! system tables of the `console` feature list every actor, bare or managed,
//! from its start until publication (`processes`, `info(id)`, keyset
//! `actors_page`, `snapshot`), alongside the tier 0 `ActorSystem::actors` /
//! `snapshot` and `ExecutionGroup::actors` / `snapshot`. Both modes.
//!
//! Tests run in parallel in one process and share the tables: each test
//! checks only the actors it started, plus order and uniqueness of whole
//! listings.
#![cfg(feature = "console")]
#![cfg_attr(
    any(spawned_sim, not(feature = "async")),
    allow(unused_imports, dead_code)
)]

use spawned_concurrency::introspection::{self, ActorInfo, Lifecycle, Placement, MAX_PAGE};
use spawned_concurrency::message::Message;
use spawned_concurrency::{ActorId, ExitReason, PendingReply};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(10);

struct Ping;
impl Message for Ping {
    type Result = ();
}

fn strictly_increasing(ids: &[ActorId], what: &str) {
    for pair in ids.windows(2) {
        assert!(
            pair[0] < pair[1],
            "{what}: {:?} before {:?}",
            pair[0],
            pair[1]
        );
    }
}

fn listed(id: ActorId) -> bool {
    introspection::processes().binary_search(&id).is_ok()
}

/// Walk the whole listing in pages of `limit`.
fn walk(limit: usize) -> Vec<ActorInfo> {
    let mut all = Vec::new();
    let mut after = None;
    loop {
        let page = introspection::actors_page(after, limit);
        assert!(page.actors.len() <= limit.clamp(1, MAX_PAGE));
        if let Some(next) = page.next {
            assert_eq!(Some(next), page.actors.last().map(|a| a.id));
        }
        all.extend(page.actors);
        match page.next {
            Some(next) => after = Some(next),
            None => return all,
        }
    }
}

fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + LIMIT;
    while !done() {
        assert!(Instant::now() < until, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(all(feature = "async", not(spawned_sim)))]
mod tasks_mode {
    use super::*;
    use spawned_concurrency::introspection::{Field, Unavailable};
    use spawned_concurrency::supervision::{ChildSpec, SupervisorConfig, TreeNode};
    use spawned_concurrency::system::ActorSystem;
    use spawned_concurrency::tasks::{Actor, ActorRef, ActorStart, Backend, Context, Handler};
    use std::sync::mpsc as std_mpsc;

    struct Echo;
    impl Actor for Echo {}
    impl Handler<Ping> for Echo {
        async fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }

    fn ids<A: Actor>(actors: &[&ActorRef<A>]) -> Vec<ActorId> {
        actors.iter().map(|a| a.id()).collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bare_and_managed_actors_are_listed_until_publication() {
        let system = ActorSystem::new().unwrap();
        let pool = system.async_group("list-async", 2).unwrap();
        let threads = system.thread_group("list-threads", 4).unwrap();
        let bare_async = Echo.with_options(Backend::Async).spawn().unwrap();
        let bare_blocking = Echo.with_options(Backend::Blocking).spawn().unwrap();
        let bare_thread = Echo.with_options(Backend::Thread).spawn().unwrap();
        let in_pool = Echo.with_options(&pool).spawn().unwrap();
        let in_threads = Echo.with_options(&threads).spawn().unwrap();
        let mut builder = system
            .supervisor(SupervisorConfig::default().name("list-root"))
            .unwrap();
        let child = builder
            .add(ChildSpec::new("w", &pool, |_| async {
                Ok::<_, String>(Echo)
            }))
            .unwrap();
        let root = builder.start().await.unwrap();
        let supervised = child.wait_ready().await.unwrap();
        let bare = ids(&[&bare_async, &bare_blocking, &bare_thread]);
        let managed = ids(&[&in_pool, &in_threads, &supervised]);
        for actor in [
            &bare_async,
            &bare_blocking,
            &bare_thread,
            &in_pool,
            &in_threads,
        ] {
            actor.request(Ping).await.unwrap();
        }

        // processes() and info(id): every actor, sorted.
        let all = introspection::processes();
        strictly_increasing(&all, "processes");
        for id in bare.iter().chain(&managed) {
            assert!(all.binary_search(id).is_ok(), "{id} not listed");
            let info = introspection::info(*id).unwrap();
            assert_eq!(info.id, *id);
        }
        assert_eq!(
            introspection::info(bare_blocking.id()).unwrap().placement,
            Placement::Blocking
        );
        assert_eq!(
            introspection::info(in_threads.id()).unwrap().placement,
            Placement::Group {
                name: "list-threads".into()
            }
        );

        // The process-wide snapshot: bare actors are unmanaged, managed ones
        // are under their system's groups, and the root supervisor's tree.
        let snapshot = introspection::snapshot();
        assert_eq!(snapshot.schema, introspection::SCHEMA);
        let unmanaged: Vec<ActorId> = snapshot.unmanaged.iter().map(|a| a.id).collect();
        strictly_increasing(&unmanaged, "unmanaged");
        for id in &bare {
            assert!(unmanaged.contains(id), "{id} not unmanaged");
        }
        for id in &managed {
            assert!(!unmanaged.contains(id), "{id} listed as unmanaged");
        }
        let ours = snapshot
            .systems
            .iter()
            .find(|s| s.id == system.id())
            .expect("our system in the snapshot");
        let group_names: Vec<&str> = ours.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(group_names, ["list-async", "list-threads"]);
        let pool_ids: Vec<ActorId> = ours.groups[0].actors.iter().map(|a| a.id).collect();
        strictly_increasing(&pool_ids, "group");
        assert!(pool_ids.contains(&in_pool.id()) && pool_ids.contains(&supervised.id()));
        assert_eq!(
            ours.groups[1]
                .actors
                .iter()
                .map(|a| a.id)
                .collect::<Vec<_>>(),
            [in_threads.id()]
        );
        assert_eq!(ours.groups[1].worker_budget, 4);
        assert_eq!(
            ours.groups[1].busy_workers,
            Field::Unavailable(Unavailable::Feature)
        );
        assert_eq!(ours.supervisors.len(), 1);
        let tree = &ours.supervisors[0];
        assert_eq!(tree.name, "list-root");
        match &tree.children[0].node {
            TreeNode::Worker(Field::Value(info)) => assert_eq!(info.id, supervised.id()),
            other => panic!("unexpected node {other:?}"),
        }

        // The tier 0 listings agree on the managed actors.
        let system_ids: Vec<ActorId> = system.actors().iter().map(|a| a.id).collect();
        strictly_increasing(&system_ids, "system.actors");
        assert_eq!(
            system_ids.iter().copied().collect::<BTreeSet<_>>(),
            managed.iter().copied().collect::<BTreeSet<_>>()
        );
        assert_eq!(system.snapshot().groups.len(), 2);
        assert_eq!(threads.actors().len(), 1);
        assert_eq!(pool.snapshot().unwrap().name, "list-async");

        // The JSON form: one line, every id a string.
        let json = snapshot.to_json();
        assert!(!json.contains('\n'));
        assert!(json.starts_with(r#"{"schema":1,"taken_at_ms":""#), "{json}");
        for id in bare.iter().chain(&managed) {
            assert!(
                json.contains(&format!(r#""id":"{id}""#)),
                "{id} missing: {json}"
            );
        }

        // Published actors leave every listing.
        for actor in [
            &bare_async,
            &bare_blocking,
            &bare_thread,
            &in_pool,
            &in_threads,
        ] {
            actor.child_handle().stop();
            assert_eq!(actor.wait_exit().await, ExitReason::Normal);
            assert!(introspection::info(actor.id()).is_none());
            assert!(!listed(actor.id()));
        }
        assert_eq!(
            system.actors().iter().map(|a| a.id).collect::<Vec<_>>(),
            [supervised.id()]
        );
        let report = root.shutdown(LIMIT).await;
        assert!(report.complete);
        assert!(supervised.exit_reason().is_some());
        assert!(introspection::info(supervised.id()).is_none());
        let id = system.id();
        assert!(system.shutdown(LIMIT).await.complete);
        drop(system);
        eventually("the system leaves the snapshot", || {
            introspection::snapshot().systems.iter().all(|s| s.id != id)
        });
    }

    /// Between D and publication the actor is listed as
    /// `Dead { published: false }` and `is_alive()` is false; once
    /// `wait_exit()` is ready it is unlisted.
    struct SlowDrop(Option<std_mpsc::Receiver<()>>);
    impl Actor for SlowDrop {}
    impl Drop for SlowDrop {
        fn drop(&mut self) {
            if let Some(gate) = self.0.take() {
                let _ = gate.recv_timeout(LIMIT);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn d_to_publication_interval() {
        let (release, gate) = std_mpsc::channel();
        let actor = SlowDrop(Some(gate)).spawn();
        let handle = actor.child_handle();
        assert!(handle.is_alive());
        handle.stop();
        let id = actor.id();
        eventually("dead, not published", || {
            matches!(
                introspection::info(id).map(|i| i.lifecycle),
                Some(Lifecycle::Dead {
                    published: false,
                    ..
                })
            )
        });
        assert!(listed(id));
        assert!(!handle.is_alive(), "is_alive() is false from D");
        assert!(handle.exit_reason().is_none(), "not published yet");
        let row = walk(MAX_PAGE).into_iter().find(|a| a.id == id).unwrap();
        assert_eq!(
            row.lifecycle,
            Lifecycle::Dead {
                reason: ExitReason::Normal,
                published: false
            }
        );
        release.send(()).unwrap();
        assert_eq!(actor.wait_exit().await, ExitReason::Normal);
        assert!(introspection::info(id).is_none());
        assert!(!listed(id));
        assert!(!handle.is_alive());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pages_cover_every_actor_once_in_id_order() {
        let actors: Vec<_> = (0..25).map(|_| Echo.spawn()).collect();
        for limit in [1, 7, 25, MAX_PAGE] {
            let rows = walk(limit);
            let ids: Vec<ActorId> = rows.iter().map(|a| a.id).collect();
            strictly_increasing(&ids, "pages");
            for actor in &actors {
                assert_eq!(ids.iter().filter(|id| **id == actor.id()).count(), 1);
            }
        }
        // A zero limit reads as one; a large one is capped.
        assert!(introspection::actors_page(None, 0).actors.len() <= 1);
        assert!(introspection::actors_page(None, usize::MAX).actors.len() <= MAX_PAGE);
        // Starting after the last id ends the listing.
        let last = *introspection::processes().last().unwrap();
        let page = introspection::actors_page(Some(last), 10);
        assert!(page.actors.iter().all(|a| a.id > last));
        for actor in &actors {
            actor.child_handle().stop();
            actor.join().await;
        }
    }

    struct Churn;
    impl Actor for Churn {}
    impl Handler<Ping> for Churn {
        async fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }

    /// I6 under churn, both modes: while tasks and threads actors are
    /// started and stopped concurrently, every listing is in strictly
    /// increasing id order, the stable actors appear in each exactly once,
    /// and each row reads one actor.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn listing_under_churn() {
        use spawned_concurrency::threads::ActorStart as _;
        let stable_tasks: Vec<_> = (0..8).map(|_| Churn.spawn()).collect();
        let stable_threads: Vec<_> = (0..8).map(|_| super::threads_mode::Churn.spawn()).collect();
        let stable: BTreeSet<ActorId> = stable_tasks
            .iter()
            .map(|a| a.id())
            .chain(stable_threads.iter().map(|a| a.id()))
            .collect();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Stops the churn however the test ends, so a failing reader
        // reports instead of leaving the churners spinning.
        struct StopOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for StopOnDrop {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let _stop_guard = StopOnDrop(stop.clone());
        let mut churn = Vec::new();
        for _ in 0..2 {
            let stop = stop.clone();
            churn.push(tokio::spawn(async move {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let a = Churn.spawn();
                    a.request(Ping).await.unwrap();
                    a.child_handle().stop();
                    a.join().await;
                }
            }));
        }
        let mut native = Vec::new();
        for _ in 0..2 {
            let stop = stop.clone();
            native.push(std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let a = super::threads_mode::Churn.spawn();
                    a.request(Ping).unwrap();
                    a.child_handle().stop();
                    a.join();
                }
            }));
        }
        let reader = tokio::task::spawn_blocking(move || {
            let check = |ids: Vec<ActorId>, what: &str| {
                strictly_increasing(&ids, what);
                let seen: BTreeSet<ActorId> = ids.into_iter().collect();
                assert!(stable.is_subset(&seen), "{what}: a stable actor is missing");
            };
            // Until the listings have seen plenty of churn come and go.
            let until = Instant::now() + LIMIT;
            let mut churned = BTreeSet::new();
            let mut rounds = 0;
            while rounds < 200 || churned.len() < 20 {
                assert!(
                    Instant::now() < until,
                    "too little churn: {}",
                    churned.len()
                );
                rounds += 1;
                check(introspection::processes(), "processes");
                let rows = walk(16);
                for row in &rows {
                    if row.actor_type.ends_with("::Churn") && !stable.contains(&row.id) {
                        churned.insert(row.id);
                    }
                    // One actor per row: its type decides its placement.
                    if row.actor_type.ends_with("threads_mode::Churn") {
                        assert_eq!(row.placement, Placement::Native);
                    } else if row.actor_type.ends_with("tasks_mode::Churn") {
                        assert_eq!(row.placement, Placement::Async);
                    }
                    if stable.contains(&row.id) {
                        assert!(
                            !matches!(row.lifecycle, Lifecycle::Dead { .. }),
                            "stable actor {} read dead",
                            row.id
                        );
                    }
                }
                check(rows.iter().map(|a| a.id).collect(), "pages");
                check(
                    introspection::snapshot()
                        .unmanaged
                        .iter()
                        .map(|a| a.id)
                        .collect(),
                    "snapshot",
                );
            }
        });
        reader.await.unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for task in churn {
            task.await.unwrap();
        }
        for thread in native {
            thread.join().unwrap();
        }
        for actor in &stable_tasks {
            actor.child_handle().stop();
            actor.join().await;
        }
        for actor in &stable_threads {
            actor.child_handle().stop();
            actor.join();
        }
    }
}

mod threads_mode {
    use super::*;
    use spawned_concurrency::threads::{Actor, ActorStart, Context, Handler};

    struct Echo;
    impl Actor for Echo {}
    impl Handler<Ping> for Echo {
        fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }

    /// The threads-mode actor of `tasks_mode::listing_under_churn`.
    pub(crate) struct Churn;
    impl Actor for Churn {}
    impl Handler<Ping> for Churn {
        fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }

    #[test]
    fn threads_actors_are_listed_until_publication() {
        let actor = Echo.spawn();
        actor.request(Ping).unwrap();
        let id = actor.id();
        assert!(listed(id));
        assert_eq!(
            introspection::info(id).unwrap().placement,
            Placement::Native
        );
        // The reply is sent inside the turn; the turn ends just after.
        eventually("idle", || {
            introspection::info(id).unwrap().lifecycle == Lifecycle::Idle
        });
        let snapshot = introspection::snapshot();
        assert!(snapshot.unmanaged.iter().any(|a| a.id == id));
        assert!(walk(5).iter().any(|a| a.id == id));
        actor.child_handle().stop();
        assert_eq!(actor.wait_exit(), ExitReason::Normal);
        assert!(introspection::info(id).is_none());
        assert!(!listed(id));
    }
}

/// Listing reads under the simulator: they follow spawn and publication on
/// every seed, and they take no scheduling decision.
#[cfg(feature = "async")]
mod seeded {
    use super::*;
    use spawned_concurrency::system::ActorSystem;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};

    struct Echo;
    impl Actor for Echo {}
    impl Handler<Ping> for Echo {
        async fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            reply.reply(());
        }
    }

    #[spawned_concurrency::sim_test]
    async fn listing_follows_spawn_and_publication() {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("seeded", 1).unwrap();
        let bare = Echo.spawn();
        let managed = Echo.with_options(&group).spawn().unwrap();
        bare.request(Ping).await.unwrap();
        managed.request(Ping).await.unwrap();
        assert!(listed(bare.id()) && listed(managed.id()));
        assert_eq!(
            system.actors().iter().map(|a| a.id).collect::<Vec<_>>(),
            [managed.id()]
        );
        let snapshot = introspection::snapshot();
        assert!(snapshot.unmanaged.iter().any(|a| a.id == bare.id()));
        assert!(snapshot.unmanaged.iter().all(|a| a.id != managed.id()));
        let ours = snapshot
            .systems
            .iter()
            .find(|s| s.id == system.id())
            .unwrap();
        assert_eq!(ours.groups[0].actors[0].id, managed.id());
        for actor in [&bare, &managed] {
            actor.child_handle().stop();
            assert_eq!(actor.wait_exit().await, ExitReason::Normal);
            assert!(introspection::info(actor.id()).is_none());
        }
        assert!(system.actors().is_empty());
        assert!(system.shutdown(Duration::from_secs(5)).await.complete);
    }
}

/// I1 and I12 for listings: `processes`, `actors_page`, `snapshot` and its
/// JSON leave a seed's decision tape and schedule unchanged.
#[cfg(all(spawned_sim, feature = "async"))]
mod sim_mode {
    use super::*;
    use spawned_concurrency::system::ActorSystem;
    use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler};
    use spawned_rt::tasks::sim;

    struct Node;
    impl Actor for Node {}
    impl Handler<Ping> for Node {
        async fn handle(&mut self, _: Ping, reply: PendingReply<'_, ()>, _: &Context<Self>) {
            spawned_rt::tasks::sleep(Duration::from_millis(1)).await;
            reply.reply(());
        }
    }

    async fn body(read: bool) {
        let system = ActorSystem::new().unwrap();
        let group = system.async_group("tape", 1).unwrap();
        let a = Node.spawn();
        let b = Node.with_options(&group).spawn().unwrap();
        let read_all = || {
            if read {
                let _ = introspection::processes();
                let _ = introspection::actors_page(None, 3);
                let _ = introspection::info(a.id());
                let _ = introspection::snapshot().to_json();
                let _ = (system.actors(), system.snapshot(), group.snapshot());
            }
        };
        for _ in 0..4 {
            read_all();
            let _ = a.send(Ping);
            spawned_rt::tasks::yield_now().await;
            read_all();
            b.request(Ping).await.unwrap();
        }
        a.child_handle().stop();
        read_all();
        a.join().await;
        read_all();
        assert!(system.shutdown(Duration::from_secs(5)).await.complete);
        read_all();
    }

    #[test]
    fn listings_do_not_change_the_tape() {
        for seed in 0..16 {
            let without = sim::run_once(seed, None, &|_| body(false));
            let with = sim::run_once(seed, None, &|_| body(true));
            assert_eq!(without.failure, None, "seed {seed}");
            assert_eq!(with.failure, None, "seed {seed}");
            assert_eq!(without.tape, with.tape, "seed {seed}");
            assert_eq!(
                without.stats.schedule_hash, with.stats.schedule_hash,
                "seed {seed}"
            );
        }
    }
}
