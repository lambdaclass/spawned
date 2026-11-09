//! Controlled handler-progress simulation, not a replacement Tokio/OS scheduler.
#[path = "support/failure_actor.rs"]
mod actor;
use actor::*;
use spawned_concurrency::{ActorError, ExitReason};
use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct State {
    completed: [u8; 3],
    stopped: [bool; 3],
}
const INITIAL: State = State {
    completed: [0; 3],
    stopped: [false; 3],
};
#[derive(Clone, Copy, Debug)]
enum Action {
    Grant(usize),
    Stop(usize),
}
impl State {
    fn actions(self) -> Vec<Action> {
        let mut actions = Vec::new();
        for actor in 0..3 {
            if !self.stopped[actor] {
                if self.completed[actor] < 2 {
                    actions.push(Action::Grant(actor));
                }
                actions.push(Action::Stop(actor));
            }
        }
        actions
    }
    fn apply(mut self, action: Action) -> Self {
        let actor = match action {
            Action::Grant(actor) | Action::Stop(actor) => actor,
        };
        assert!(!self.stopped[actor]);
        match action {
            Action::Grant(_) => {
                assert!(self.completed[actor] < 2);
                self.completed[actor] += 1;
            }
            Action::Stop(_) => {
                // Every nonempty actor has already entered its current handler.
                // Cooperative stop lets that handler finish; queued work is discarded.
                self.completed[actor] = (self.completed[actor] + 1).min(2);
                self.stopped[actor] = true;
            }
        }
        assert!(self.completed.iter().all(|&count| count <= 2));
        self
    }
}
struct World<'a> {
    rt: &'a tokio::runtime::Runtime,
    actors: Vec<Handle>,
    controls: Vec<Vec<Control>>,
    replies: Vec<Vec<Option<Response>>>,
    effects: Vec<Arc<Mutex<Vec<u64>>>>,
    _cleanup: Cleanup,
    state: State,
    digest: u64,
}
impl<'a> World<'a> {
    fn new(rt: &'a tokio::runtime::Runtime, backend: Backend) -> Self {
        let mut cleanup = Cleanup::default();
        let mut actors = Vec::new();
        let mut controls = Vec::new();
        let mut replies = Vec::new();
        let mut effects = Vec::new();
        for slot in 0..3 {
            let observed = Arc::new(Mutex::new(Vec::new()));
            let actor = Handle::start(
                backend,
                Worker {
                    effects: Some(observed.clone()),
                    ..Worker::default()
                },
            );
            cleanup.track(&actor);
            let mut pauses = Vec::new();
            let mut pending = Vec::new();
            for message in 0..2 {
                let (before, control) = gate();
                pending.push(Some(
                    actor
                        .raw(Command::Append {
                            id: slot * 2 + message,
                            before: Some(before),
                            after: None,
                            panic_after: false,
                        })
                        .unwrap(),
                ));
                pauses.push(control);
            }
            pauses[0].entered();
            actors.push(actor);
            controls.push(pauses);
            replies.push(pending);
            effects.push(observed);
        }
        Self {
            rt,
            actors,
            controls,
            replies,
            effects,
            _cleanup: cleanup,
            state: INITIAL,
            digest: 0xcbf29ce484222325,
        }
    }
    fn step(&mut self, action: Action, tick: u64) {
        let slot = match action {
            Action::Grant(slot) | Action::Stop(slot) => slot,
        };
        let before = self.state.completed[slot] as usize;
        if matches!(action, Action::Stop(_)) {
            self.actors[slot].stop();
        }
        if before < 2 {
            self.controls[slot][before].release();
            assert_eq!(
                self.replies[slot][before]
                    .take()
                    .unwrap()
                    .wait(self.rt, LIMIT)
                    .unwrap(),
                Reply::Appended {
                    id: (slot * 2 + before) as u64,
                    length: before + 1
                }
            );
        }
        self.state = self.state.apply(action);
        if self.state.stopped[slot] {
            assert_eq!(self.actors[slot].wait(self.rt), ExitReason::Normal);
            for pending in self.replies[slot].iter_mut().skip(before + 1) {
                if let Some(reply) = pending.take() {
                    assert!(matches!(
                        reply.wait(self.rt, LIMIT),
                        Err(ActorError::ActorStopped)
                    ));
                }
            }
        } else if self.state.completed[slot] < 2 {
            self.controls[slot][self.state.completed[slot] as usize].entered();
        }
        for actor in 0..3 {
            let expected: Vec<_> = (0..self.state.completed[actor])
                .map(|id| actor as u64 * 2 + id as u64)
                .collect();
            assert_eq!(*self.effects[actor].lock().unwrap(), expected);
        }
        // Fixed non-cryptographic audit fingerprint; excludes wall time and ActorId.
        let kind = if matches!(action, Action::Stop(_)) {
            1
        } else {
            0
        };
        for byte in tick
            .to_le_bytes()
            .into_iter()
            .chain([slot as u8, kind])
            .chain(self.state.completed)
            .chain(self.state.stopped.map(u8::from))
        {
            self.digest = (self.digest ^ byte as u64).wrapping_mul(0x100000001b3);
        }
    }
    fn finish(mut self) -> u64 {
        for slot in 0..3 {
            if !self.state.stopped[slot] {
                self.step(Action::Stop(slot), 100 + slot as u64);
            }
        }
        self.digest
    }
}

#[test]
fn finite_model_witnesses_replay_against_all_execution_modes() {
    let mut visited = HashSet::from([INITIAL]);
    let mut queue = VecDeque::from([(INITIAL, Vec::new())]);
    let mut witnesses = Vec::new();
    while let Some((state, path)) = queue.pop_front() {
        witnesses.push((state, path.clone()));
        for action in state.actions() {
            let next = state.apply(action);
            if visited.insert(next) {
                let mut path = path.clone();
                path.push(action);
                queue.push_back((next, path));
            }
        }
    }
    assert_eq!(
        visited.len(),
        125,
        "finite model changed; review the state boundary"
    );
    let rt = runtime();
    let _entered = rt.enter();
    for &backend in BACKENDS {
        for (expected, path) in &witnesses {
            let mut world = World::new(&rt, backend);
            for (tick, action) in path.iter().enumerate() {
                world.step(*action, tick as u64);
            }
            assert_eq!(world.state, *expected);
            world.finish();
        }
    }
    println!("125 bounded abstract states; 500 real-backend witness replays; no claim of exhaustive Rust schedules");
}

#[test]
fn seeded_handler_interleavings_have_identical_audit_fingerprints() {
    let cases: usize = std::env::var("SPAWNED_SIM_CASES")
        .unwrap_or_else(|_| "16".into())
        .parse()
        .unwrap();
    assert!((1..=4096).contains(&cases));
    let first: u64 = std::env::var("SPAWNED_SIM_SEED")
        .unwrap_or_else(|_| "20261106".into())
        .parse()
        .unwrap();
    let rt = runtime();
    let _entered = rt.enter();
    for seed in first..first.checked_add(cases as u64).unwrap() {
        let mut rng = seed;
        let mut state = INITIAL;
        let mut script = Vec::new();
        let mut tick = 0;
        while !state.actions().is_empty() {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let actions = state.actions();
            let action = actions[(rng >> 32) as usize % actions.len()];
            tick += (rng & 7) + 1;
            script.push((tick, action));
            state = state.apply(action);
        }
        let mut fingerprints = Vec::new();
        for &backend in BACKENDS {
            let mut world = World::new(&rt, backend);
            for &(tick, action) in &script {
                world.step(action, tick);
            }
            assert_eq!(world.state, state);
            fingerprints.push(world.finish());
        }
        assert!(
            fingerprints.windows(2).all(|pair| pair[0] == pair[1]),
            "seed={seed} script={script:?} fingerprints={fingerprints:?}"
        );
    }
    println!("{cases} seeded scripts × four backends; virtual ticks order commands, not arbitrary I/O or await points");
}
