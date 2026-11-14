// `#[deferred_handler]` in both modes: answer in place, or defer and answer
// from a later turn; a `ReplyTo` is storable and `Send + 'static`.
use spawned_concurrency::{actor, protocol, DeferError, PendingReply, ReplyTo, Response};

#[protocol]
pub trait WaitProtocol: Send + Sync {
    fn wait(&self) -> Response<u64>;
    fn release(&self, value: u64);
}
use wait_protocol::{Release, Wait};

mod tasks_mode {
    use super::*;
    use spawned_concurrency::tasks::{Actor, Context, Handler};
    pub struct Gate {
        waiters: Vec<ReplyTo<u64>>,
        ready: Option<u64>,
    }
    #[actor(protocol = WaitProtocol)]
    impl Gate {
        #[deferred_handler]
        async fn wait(&mut self, _msg: Wait, reply: PendingReply<'_, u64>, _ctx: &Context<Self>) {
            if let Some(value) = self.ready {
                reply.reply(value);
                return;
            }
            match reply.defer() {
                Ok(token) => self.waiters.push(token),
                Err(DeferError::Overloaded | DeferError::Closed | DeferError::NoCaller) => {}
                // `DeferError` is `#[non_exhaustive]`.
                Err(_) => {}
            }
        }
        #[send_handler]
        async fn release(&mut self, msg: Release, _ctx: &Context<Self>) {
            self.ready = Some(msg.value);
            for token in self.waiters.drain(..) {
                token.reply(msg.value);
            }
        }
    }
}

mod threads_mode {
    use super::*;
    use spawned_concurrency::threads::{Actor, Context, Handler};
    pub struct Gate {
        waiters: Vec<ReplyTo<u64>>,
    }
    #[actor(protocol = WaitProtocol)]
    impl Gate {
        #[deferred_handler]
        fn wait(&mut self, _msg: Wait, reply: PendingReply<'_, u64>, _ctx: &Context<Self>) {
            if let Ok(token) = reply.defer() {
                self.waiters.push(token);
            }
        }
        #[send_handler]
        fn release(&mut self, msg: Release, _ctx: &Context<Self>) {
            for token in self.waiters.drain(..) {
                token.reply(msg.value);
            }
        }
    }
}

fn assert_storable<T: Send + 'static>() {}

fn main() {
    assert_storable::<ReplyTo<u64>>();
}
