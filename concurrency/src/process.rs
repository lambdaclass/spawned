//! Process trait and struct to create a process abstraction similar to Erlang processes.
//! See examples/ping_pong for a usage example.

use std::future::Future;

use spawned_rt::{self as rt, JoinHandle, mpsc};

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
