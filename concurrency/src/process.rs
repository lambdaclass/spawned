//! Simple process abstraction for message passing.
//!
//! This module provides a lightweight [`Process`] trait for creating concurrent
//! message-handling processes, similar to Erlang processes.
//!
//! # Overview
//!
//! The [`Process`] trait provides:
//! - Automatic message loop
//! - Initialization callback
//! - Message handling callback
//! - Graceful shutdown via `should_stop()`
//!
//! # Example
//!
//! ```ignore
//! use spawned_concurrency::{Process, ProcessInfo, send};
//!
//! struct Echo {
//!     stopped: bool,
//! }
//!
//! impl Process<String> for Echo {
//!     fn should_stop(&self) -> bool {
//!         self.stopped
//!     }
//!
//!     async fn handle(&mut self, message: String, tx: &Sender<String>) -> String {
//!         if message == "STOP" {
//!             self.stopped = true;
//!         } else {
//!             let _ = tx.send(message.clone());
//!         }
//!         message
//!     }
//! }
//!
//! // Spawn and send messages
//! let info = Echo { stopped: false }.spawn().await;
//! send(&info.tx, "hello".to_string());
//! send(&info.tx, "STOP".to_string());
//! info.handle.await.unwrap();
//! ```
//!
//! For more complex use cases with request-reply patterns, see [`GenServer`](crate::GenServer).

use spawned_rt::tasks::{self as rt, mpsc, JoinHandle};
use std::future::Future;

#[derive(Debug)]
pub struct ProcessInfo<T> {
    pub tx: mpsc::Sender<T>,
    pub handle: JoinHandle<()>,
}

impl<T> ProcessInfo<T> {
    pub fn sender(&self) -> mpsc::Sender<T> {
        self.tx.clone()
    }

    pub fn handle(self) -> JoinHandle<()> {
        self.handle
    }
}

pub trait Process<T: Send + 'static>
where
    Self: Send + Sync + Sized + 'static,
{
    fn spawn(mut self) -> impl Future<Output = ProcessInfo<T>> + Send {
        async {
            let (tx, mut rx) = mpsc::channel::<T>();
            let tx_clone = tx.clone();
            let handle = rt::spawn(async move {
                self.run(&tx_clone, &mut rx).await;
            });
            ProcessInfo { tx, handle }
        }
    }

    fn run(
        &mut self,
        tx: &mpsc::Sender<T>,
        rx: &mut mpsc::Receiver<T>,
    ) -> impl Future<Output = ()> + Send {
        async {
            self.init(tx).await;
            self.main_loop(tx, rx).await;
        }
    }

    fn main_loop(
        &mut self,
        tx: &mpsc::Sender<T>,
        rx: &mut mpsc::Receiver<T>,
    ) -> impl Future<Output = ()> + Send {
        async {
            loop {
                if self.should_stop() {
                    break;
                }

                self.receive(tx, rx).await;
            }
        }
    }

    fn should_stop(&self) -> bool {
        false
    }

    fn init(&mut self, _tx: &mpsc::Sender<T>) -> impl Future<Output = ()> + Send {
        async {}
    }

    fn receive(
        &mut self,
        tx: &mpsc::Sender<T>,
        rx: &mut mpsc::Receiver<T>,
    ) -> impl std::future::Future<Output = T> + Send {
        async {
            match rx.recv().await {
                Some(message) => self.handle(message, tx).await,
                None => todo!(),
            }
        }
    }

    fn handle(&mut self, message: T, tx: &mpsc::Sender<T>) -> impl Future<Output = T> + Send;
}

pub fn send<T>(tx: &mpsc::Sender<T>, message: T)
where
    T: Send,
{
    let _ = tx.send(message);
}
