//! Process trait and struct to create a process abstraction similar to Erlang processes.
//! See examples/ping_pong for a usage example.

use spawned_rt::threads::{self as rt, mpsc, JoinHandle};

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
    fn spawn(mut self) -> ProcessInfo<T> {
        let (tx, mut rx) = mpsc::unbounded_channel::<T>();
        let tx_clone = tx.clone();
        let handle = rt::spawn(move || self.run(&tx_clone, &mut rx));
        ProcessInfo { tx, handle }
    }

    fn run(&mut self, tx: &mpsc::Sender<T>, rx: &mut mpsc::Receiver<T>) {
        self.init(tx);
        self.main_loop(tx, rx);
    }

    fn main_loop(&mut self, tx: &mpsc::Sender<T>, rx: &mut mpsc::Receiver<T>) {
        loop {
            if self.should_stop() {
                break;
            }

            self.receive(tx, rx);
        }
    }

    fn should_stop(&self) -> bool {
        false
    }

    fn init(&mut self, _tx: &mpsc::Sender<T>) {
        {}
    }

    fn receive(&mut self, tx: &mpsc::Sender<T>, rx: &mut mpsc::Receiver<T>) -> T {
        match rx.recv().ok() {
            Some(message) => self.handle(message, tx),
            None => todo!(),
        }
    }

    fn handle(&mut self, message: T, tx: &mpsc::Sender<T>) -> T;
}

pub fn send<T>(tx: &mpsc::Sender<T>, message: T)
where
    T: Send,
{
    let _ = tx.send(message);
}
