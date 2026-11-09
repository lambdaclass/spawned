//! Per-message-type rules shared by the `tasks` and `threads` mocks.
use crate::inbox::{Inbox, Outcome};
use spawned_concurrency::message::Message;
use std::any::{Any, TypeId};
use std::collections::HashMap;
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
    Reply { delay: Option<Duration>, value: R },
    Crash(String),
}

#[derive(Default)]
pub(crate) struct Rules {
    by_type: HashMap<TypeId, Box<dyn Any + Send>>,
    allow_sends: bool,
}

/// `Some(())` as `M::Result` when that type is `()`: lets one generic handler
/// acknowledge fire-and-forget messages without a rule.
fn unit_result<M: Message>() -> Option<M::Result> {
    let unit: Box<dyn Any> = Box::new(());
    unit.downcast::<M::Result>().ok().map(|b| *b)
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
    pub(crate) fn allow_sends(&mut self) {
        self.allow_sends = true;
    }

    /// Log the message and decide the reply. The reply closure runs before any
    /// configured delay; the caller only observes the reply after it.
    pub(crate) fn plan<M: Message>(&mut self, inbox: &Inbox, msg: M) -> Plan<M::Result> {
        if TypeId::of::<M>() == TypeId::of::<Fence>() {
            return Plan::Reply {
                delay: None,
                value: unit_result::<M>().expect("Fence replies with ()"),
            };
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
                    inbox.push(msg, Outcome::Handled);
                    return Plan::Reply { delay: None, value };
                }
            }
            inbox.push(msg, Outcome::Unexpected);
            return Plan::Crash(format!(
                "spawned-test Mock: no rule for message `{name}`; configure it with \
                 `.on::<{name}>(..)`{}",
                if unit_result::<M>().is_some() {
                    ", acknowledge it with `.allow_send::<{name}>()` or acknowledge every \
                     `()`-reply message with `.allow_sends()`"
                } else {
                    ""
                }
            ));
        };
        let delay = rule.delay;
        if let Some(reason) = rule.crash.clone() {
            inbox.push(msg, Outcome::Crashed);
            return Plan::Crash(reason);
        }
        if rule.discard {
            inbox.push(msg, Outcome::Discarded);
            let value = unit_result::<M>().expect("discard requires Result = ()");
            return Plan::Reply { delay, value };
        }
        let value = match rule.reply.as_mut() {
            Some(reply) => reply(&msg),
            None => match unit_result::<M>() {
                Some(value) => value,
                None => {
                    inbox.push(msg, Outcome::Unexpected);
                    return Plan::Crash(format!(
                        "spawned-test Mock: request `{name}` has a delay but no reply; \
                         add `.on::<{name}>(..)`"
                    ));
                }
            },
        };
        inbox.push(msg, Outcome::Handled);
        Plan::Reply { delay, value }
    }
}
