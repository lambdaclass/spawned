//! Items identical in the `tasks` and `threads` modules, expanded in each.
//!
//! The two modes differ in their actor traits, `oneshot` channels and whether
//! waits are `async`; everything else about mocks and probes is written once
//! here. Each macro resolves `Mock`, `MockRef`, `Probe`, `Recipient`,
//! `Receiver`, `oneshot` and friends in the module that invokes it.

/// Builder methods of `Mock`, except `delay` and `start`.
macro_rules! mock_builder {
    () => {
        /// A strict mock: every message type needs a rule.
        pub fn new() -> Self {
            Self {
                rules: Rules::default(),
                inbox: Arc::new(Inbox::default()),
            }
        }

        /// Answer each `M` with `reply(&msg)`. The message is still logged
        /// for `expect`. For fire-and-forget messages return `()`.
        pub fn on<M: Message>(
            mut self,
            reply: impl FnMut(&M) -> M::Result + Send + 'static,
        ) -> Self {
            self.rules.on::<M>(reply);
            self
        }

        /// Acknowledge and log each `M` (a message whose reply type is `()`)
        /// without other behavior. Shorthand for `.on::<M>(|_| ())`.
        pub fn allow_send<M: Message<Result = ()>>(self) -> Self {
            self.on::<M>(|_| ())
        }

        /// Acknowledge and log, without a rule, every message whose reply type
        /// is `()`.
        ///
        /// A handler cannot tell a send from a request through Spawned's
        /// public API, so this also answers *requests* whose reply is `()`
        /// (for example a protocol method returning `Response<()>`). Requests
        /// with any other reply type still fail loudly without a rule. Use
        /// [`allow_send`](Self::allow_send) to acknowledge chosen types only.
        pub fn allow_sends(mut self) -> Self {
            self.rules.allow_sends();
            self
        }

        /// Fault: log each `M` as [`crate::Outcome::Discarded`] and do nothing
        /// else, as if the message were lost after a successful send.
        pub fn discard<M: Message<Result = ()>>(mut self) -> Self {
            self.rules.discard::<M>();
            self
        }

        /// Fault: panic with `reason` on each `M`. The mock exits with
        /// `ExitReason::Panic` and pending callers see `ActorStopped`.
        pub fn crash_on<M: Message>(mut self, reason: impl Into<String>) -> Self {
            self.rules.crash_on::<M>(reason.into());
            self
        }
    };
}

/// `Deref` and the non-waiting log readers of `MockRef`.
macro_rules! mock_ref_common {
    () => {
        impl Deref for MockRef {
            type Target = ActorRef<Mock>;
            fn deref(&self) -> &ActorRef<Mock> {
                &self.actor
            }
        }

        impl MockRef {
            fn who(&self) -> String {
                format!("Mock {:?}", self.actor.id())
            }

            fn settle_failed(&self, error: ActorError) -> ! {
                panic!(
                    "{}: could not settle ({error}); exit reason {:?}, log {:?}",
                    self.who(),
                    self.actor.exit_reason(),
                    self.received()
                )
            }

            /// The underlying actor reference.
            pub fn actor_ref(&self) -> ActorRef<Mock> {
                self.actor.clone()
            }

            /// Every message received so far, in arrival order, including
            /// expected ones.
            pub fn received(&self) -> Vec<Received> {
                self.inbox.received()
            }

            /// How many `M` were received so far, including expected ones.
            pub fn count<M: Message>(&self) -> usize {
                self.inbox.count::<M>()
            }

            /// Take every not-yet-expected `M`, oldest first.
            pub fn take_all<M: Message>(&self) -> Vec<M> {
                self.inbox.take_all::<M>()
            }

            /// Panic if any message had no rule.
            pub fn assert_no_unexpected(&self) {
                self.inbox.assert_no_unexpected(&self.who());
            }
        }
    };
}

/// Everything about `Probe<M>` except its waiting methods.
macro_rules! probe_common {
    () => {
        impl<M: Message> Clone for Probe<M> {
            fn clone(&self) -> Self {
                Self {
                    core: self.core.clone(),
                }
            }
        }

        impl<M: Message> Default for Probe<M> {
            fn default() -> Self {
                Self::new()
            }
        }

        impl<M: Message> Receiver<M> for ProbeCore<M> {
            fn send(&self, msg: M) -> Result<(), ActorError> {
                ProbeCore::send(self, msg)
            }
            fn request_raw(&self, msg: M) -> Result<oneshot::Receiver<M::Result>, ActorError> {
                let (tx, rx) = oneshot::channel();
                if let Some(value) = self.request(msg)? {
                    let _ = tx.send(value);
                }
                Ok(rx)
            }
        }

        impl<M: Message> Probe<M> {
            /// A probe for fire-and-forget messages.
            pub fn new() -> Self {
                Self {
                    core: Arc::new(ProbeCore::new(None)),
                }
            }

            /// A probe that answers requests with `reply(&msg)`.
            pub fn replying(reply: impl FnMut(&M) -> M::Result + Send + 'static) -> Self {
                Self {
                    core: Arc::new(ProbeCore::new(Some(Box::new(reply)))),
                }
            }

            /// The probe as a `Recipient<M>`, to hand to the code under test.
            pub fn recipient(&self) -> Recipient<M> {
                self.core.clone()
            }

            /// Every message received so far, in arrival order, including
            /// expected ones.
            pub fn received(&self) -> Vec<Received> {
                self.core.inbox.received()
            }

            /// Take every not-yet-expected message, oldest first.
            pub fn take_all(&self) -> Vec<M> {
                self.core.inbox.take_all::<M>()
            }

            /// Fault: behave like a stopped actor; later sends and requests
            /// return `ActorStopped`.
            pub fn close(&self) {
                self.core.close();
            }
        }
    };
}

/// Default bound of `MockRef::settle`.
pub(crate) const SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
