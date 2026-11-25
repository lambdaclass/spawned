//! Listings and snapshots (docs/design/introspection.md, "Listing and
//! snapshots"): OTP's `erlang:processes/0`, `erlang:process_info/1` by pid,
//! and an `erlang:system_info`-like dump of every system, group and
//! supervision tree.
//!
//! `ActorSystem::actors`/`snapshot` and `ExecutionGroup::actors`/`snapshot`
//! are tier 0: they read the groups' scopes. The process-wide calls here
//! ([`processes`], [`info`], [`actors_page`], [`snapshot`]) read the actor and
//! system tables of the `console` feature (in `default`), so they also cover
//! bare actors (`spawn()` outside any system).
//!
//! Every listing is in `ActorId` order. An actor that is not past its death
//! point for the whole call appears exactly once; one that starts or ends
//! during it may or may not appear; one between its death point and
//! publication appears as `Dead { published: false }`; no id appears twice
//! (I6). Rows are read out of band (I1), one at a time: each row is
//! consistent with itself, the listing as a whole is not one instant.

use super::ActorInfo;
#[cfg(feature = "async")]
use super::{Field, SystemId};
#[cfg(feature = "console")]
use crate::child_handle::ActorId;
#[cfg(feature = "async")]
use std::time::Duration;

/// A system as it is now: its groups and root supervisors. Rows are sampled
/// one at a time; the snapshot is not one instant.
#[cfg(feature = "async")]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SystemSnapshot {
    pub id: SystemId,
    /// In creation order.
    pub groups: Vec<GroupSnapshot>,
    /// The root supervisors still running, each expanded with
    /// `Supervisor::tree`, in start order.
    pub supervisors: Vec<crate::supervision::SupervisorTree>,
}

/// An execution group as it is now.
#[cfg(feature = "async")]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GroupSnapshot {
    pub name: String,
    pub profile: crate::system::ExecutionProfile,
    pub worker_budget: usize,
    /// Admission closed (the system is shutting down).
    pub closing: bool,
    /// The group's attached actors not yet published, in id order.
    pub actors: Vec<ActorInfo>,
    /// Tier 1 (`introspection` feature, with the watchdog).
    pub busy_workers: Field<usize>,
    /// Tier 1 (`introspection` feature, with the watchdog).
    pub scheduling_delay: Field<Duration>,
    /// Scopes whose actor is already published while owned helpers or jobs
    /// still keep the scope alive.
    pub helpers_draining: usize,
}

/// Version of the [`Snapshot`] layout and of its JSON form. New fields may
/// appear within one version; removing or renaming one is a new version.
#[cfg(feature = "console")]
pub const SCHEMA: u32 = 1;

/// The most actors one [`actors_page`] returns.
#[cfg(feature = "console")]
pub const MAX_PAGE: usize = 1000;

/// One page of the process-wide listing ([`actors_page`]).
#[cfg(feature = "console")]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ActorPage {
    /// In id order.
    pub actors: Vec<ActorInfo>,
    /// The last id of this page when more actors follow it: pass it as
    /// `after` for the next page. `None` at the end of the listing.
    pub next: Option<ActorId>,
}

/// Every system and every bare actor of the process ([`snapshot`]).
#[cfg(feature = "console")]
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Snapshot {
    /// [`SCHEMA`].
    pub schema: u32,
    /// When the snapshot began (virtual time under the simulator).
    pub taken_at: crate::Instant,
    /// Wall-clock time when it began. Under `--cfg spawned_sim` the Unix
    /// epoch plus the virtual time since the simulation started, so a seed's
    /// snapshots stay deterministic.
    pub wall_clock: std::time::SystemTime,
    /// Live systems, in id order.
    #[cfg(feature = "async")]
    pub systems: Vec<SystemSnapshot>,
    /// Actors outside any live system, in id order: bare `spawn()`s, and
    /// managed actors still running after their system's state is gone (so
    /// every listed actor appears somewhere).
    pub unmanaged: Vec<ActorInfo>,
}

/// Every listed actor of the process, managed or bare, in id order (OTP
/// `erlang:processes/0`): from its start's commit until just before its
/// publication, so an actor is gone from here once `wait_exit()` has
/// returned. (The tier 0 `ExecutionGroup::actors` drops it at publication
/// itself, a moment later.) One shard read lock at a time; no actor is
/// read. The authoritative set: [`snapshot`] places each of these actors
/// under its system or in `unmanaged`.
#[cfg(feature = "console")]
pub fn processes() -> Vec<ActorId> {
    super::table::ids()
}

/// The actor `id` as it is now, `None` once it is published (OTP
/// `process_info/1` on a dead pid: `undefined`). Out of band, as
/// `ChildHandle::info`.
#[cfg(feature = "console")]
pub fn info(id: ActorId) -> Option<ActorInfo> {
    super::table::get(id).map(|handle| handle.info())
}

/// Up to `limit` actors (at most [`MAX_PAGE`]; `0` reads as 1) with an id
/// above `after`, in id order: keyset pagination, so nothing is kept between
/// pages and an actor alive across two pages appears exactly once. Pass the
/// page's `next` as the following call's `after`.
#[cfg(feature = "console")]
pub fn actors_page(after: Option<ActorId>, limit: usize) -> ActorPage {
    let (handles, more) = super::table::page(after, limit.clamp(1, MAX_PAGE));
    let next = more
        .then(|| handles.last().map(crate::ChildHandle::id))
        .flatten();
    ActorPage {
        actors: handles.iter().map(crate::ChildHandle::info).collect(),
        next,
    }
}

/// Every live system (its groups, their actors and its root supervision
/// trees) and every bare actor, read out of band (I1). Not one instant: each
/// row is read once, systems and groups one after the other.
#[cfg(feature = "console")]
pub fn snapshot() -> Snapshot {
    let taken_at = crate::Instant::now();
    #[cfg(not(spawned_sim))]
    let wall_clock = std::time::SystemTime::now();
    // No wall-clock read under the simulator (strict mode rejects it).
    #[cfg(spawned_sim)]
    let wall_clock = std::time::UNIX_EPOCH
        + std::time::Duration::from_nanos(spawned_rt::time::to_ticks(taken_at));
    #[cfg(feature = "async")]
    let live = super::table::systems::live();
    #[cfg(feature = "async")]
    let systems: Vec<SystemSnapshot> = live.iter().map(|system| system.snapshot()).collect();
    let unmanaged = super::table::all()
        .iter()
        .filter(|handle| {
            let meta = &handle.signal_target().meta;
            if meta.host != super::Host::Managed {
                return true;
            }
            // A managed actor is under its system, if that system is live.
            #[cfg(feature = "async")]
            {
                let system = meta.scope().and_then(|scope| scope.system_id());
                !system.is_some_and(|id| live.iter().any(|s| s.id() == id))
            }
            #[cfg(not(feature = "async"))]
            true
        })
        .map(crate::ChildHandle::info)
        .collect();
    Snapshot {
        schema: SCHEMA,
        taken_at,
        wall_clock,
        #[cfg(feature = "async")]
        systems,
        unmanaged,
    }
}
