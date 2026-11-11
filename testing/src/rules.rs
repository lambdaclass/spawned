//! Per-message-type rules shared by the `tasks` and `threads` mocks.
use crate::inbox::{Inbox, Outcome, Pending};
use spawned_concurrency::message::Message;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

type ReplyFn<M> = Box<dyn FnMut(&M) -> <M as Message>::Result + Send>;

struct Rule<M: Message> {
    reply: Option<ReplyFn<M>>,
    delay: Option<Duration>,
    discard: bool,
    crash: Option<String>,
}

impl<M: Message> Default for Rule<M> {
    fn default() -> Self {
        Self {
            reply: None,
            delay: None,
            discard: false,
            crash: None,
        }
    }
}

/// Fence used by `settle()`: answered without being logged.
pub(crate) struct Fence;
impl Message for Fence {
    type Result = ();
}

/// What the mock handler should do after the rules have looked at a message.
pub(crate) enum Plan<R> {
    /// Wait `delay`, then [`Pending::finish`] (if any) and return `value`, so
    /// the log shows the outcome only once the reply is about to be sent.
    Reply {
        delay: Option<Duration>,
        value: R,
        record: Option<Pending>,
    },
    Crash(String),
    /// `Mock::defer`: take the reply obligation and store it for the test.
    Defer,
    /// `Mock::discard`: wait `delay`, then drop the reply obligation.
    Discard { delay: Option<Duration> },
}

#[derive(Default)]
pub(crate) struct Rules {
    by_type: HashMap<TypeId, Box<dyn Any + Send>>,
    allow_sends: bool,
    deferred: std::collections::HashSet<TypeId>,
}

/// `Some(())` as `M::Result` when that type is `()`: lets one generic handler
/// acknowledge fire-and-forget messages without a rule.
fn unit_result<M: Message>() -> Option<M::Result> {
    // No allocation: downcast a stack `Option<()>` through `&mut dyn Any`.
    let mut unit = Some(());
    (&mut unit as &mut dyn Any)
        .downcast_mut::<Option<M::Result>>()
        .and_then(Option::take)
}

impl Rules {
    fn rule<M: Message>(&mut self) -> &mut Rule<M> {
        self.by_type
            .entry(TypeId::of::<M>())
            .or_insert_with(|| Box::new(Rule::<M>::default()))
            .downcast_mut::<Rule<M>>()
            .expect("rule stored under its message type id")
    }

    pub(crate) fn on<M: Message>(&mut self, reply: impl FnMut(&M) -> M::Result + Send + 'static) {
        self.rule::<M>().reply = Some(Box::new(reply));
    }
    pub(crate) fn delay<M: Message>(&mut self, delay: Duration) {
        self.rule::<M>().delay = Some(delay);
    }
    pub(crate) fn discard<M: Message<Result = ()>>(&mut self) {
        self.rule::<M>().discard = true;
    }
    pub(crate) fn crash_on<M: Message>(&mut self, reason: String) {
        self.rule::<M>().crash = Some(reason);
    }
    pub(crate) fn defer<M: Message>(&mut self) {
        self.deferred.insert(TypeId::of::<M>());
    }
    pub(crate) fn allow_sends(&mut self) {
        self.allow_sends = true;
    }

    /// Log the message and decide the reply.
    ///
    /// A message that gets a reply is logged [`Outcome::InFlight`] on arrival
    /// and resolved by the handler after the delay, through the returned
    /// [`Pending`]; `expect` only sees it then. The reply closure runs first;
    /// if it panics, the message is still logged, as [`Outcome::Crashed`].
    /// Crashes and unexpected messages are logged final right away: the
    /// handler panics next.
    pub(crate) fn plan<M: Message>(&mut self, inbox: &Arc<Inbox>, msg: M) -> Plan<M::Result> {
        if TypeId::of::<M>() == TypeId::of::<Fence>() {
            return Plan::Reply {
                delay: None,
                value: unit_result::<M>().expect("Fence replies with ()"),
                record: None,
            };
        }
        if self.deferred.contains(&TypeId::of::<M>()) {
            inbox.push(msg, Outcome::Handled);
            return Plan::Defer;
        }
        let name = std::any::type_name::<M>();
        let allow_sends = self.allow_sends;
        let rule = self
            .by_type
            .get_mut(&TypeId::of::<M>())
            .and_then(|r| r.downcast_mut::<Rule<M>>());
        let Some(rule) = rule else {
            if allow_sends {
                if let Some(value) = unit_result::<M>() {
                    let mut record = inbox.arrive(msg);
                    record.replied(Outcome::Handled);
                    return Plan::Reply {
                        delay: None,
                        value,
                        record: Some(record),
                    };
                }
            }
            inbox.push(msg, Outcome::Unexpected);
            let hint = if unit_result::<M>().is_some() {
                format!(
                    ", acknowledge it with `.allow_send::<{name}>()` or acknowledge every \
                     `()`-reply message with `.allow_sends()`"
                )
            } else {
                String::new()
            };
            return Plan::Crash(format!(
                "spawned-test Mock: no rule for message `{name}`; configure it with \
                 `.on::<{name}>(..)`{hint}"
            ));
        };
        let delay = rule.delay;
        if let Some(reason) = rule.crash.clone() {
            inbox.push(msg, Outcome::Crashed);
            return Plan::Crash(reason);
        }
        if rule.discard {
            // Discarded: the reply obligation is dropped unanswered, so a
            // request gets `ReplyDropped`, as a lost message would.
            inbox.push(msg, Outcome::Discarded);
            return Plan::Discard { delay };
        }
        if rule.reply.is_none() && unit_result::<M>().is_none() {
            inbox.push(msg, Outcome::Unexpected);
            return Plan::Crash(format!(
                "spawned-test Mock: request `{name}` has a delay but no reply; \
                 add `.on::<{name}>(..)`"
            ));
        }
        let mut record = inbox.arrive(msg);
        let value = match rule.reply.as_mut() {
            // A panic here drops `record`, which logs the message as Crashed.
            Some(reply) => reply(record.message::<M>()),
            None => unit_result::<M>().expect("checked above"),
        };
        record.replied(Outcome::Handled);
        Plan::Reply {
            delay,
            value,
            record: Some(record),
        }
    }
}
