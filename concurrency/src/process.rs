//! Process trait and struct to create a process abstraction similar to Erlang processes.
//! See examples/ping_pong for a usage example.

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    // Simple counter process for testing
    #[derive(Clone, Debug)]
    #[allow(dead_code)]
    enum CounterMsg {
        Increment,
        GetCount,
        Stop,
        Count(u32),
    }

    struct Counter {
        count: u32,
        stopped: bool,
        reply_to: Option<mpsc::Sender<CounterMsg>>,
    }

    impl Process<CounterMsg> for Counter {
        fn should_stop(&self) -> bool {
            self.stopped
        }

        fn handle(
            &mut self,
            message: CounterMsg,
            _tx: &mpsc::Sender<CounterMsg>,
        ) -> impl Future<Output = CounterMsg> + Send {
            async move {
                match message {
                    CounterMsg::Increment => {
                        self.count += 1;
                        CounterMsg::Increment
                    }
                    CounterMsg::GetCount => {
                        if let Some(ref reply_tx) = self.reply_to {
                            let _ = reply_tx.send(CounterMsg::Count(self.count));
                        }
                        CounterMsg::GetCount
                    }
                    CounterMsg::Stop => {
                        self.stopped = true;
                        CounterMsg::Stop
                    }
                    CounterMsg::Count(n) => CounterMsg::Count(n),
                }
            }
        }
    }

    #[tokio::test]
    async fn test_process_spawn() {
        let counter = Counter {
            count: 0,
            stopped: false,
            reply_to: None,
        };

        let info = counter.spawn().await;

        // Should have a valid sender
        assert!(!info.tx.is_closed());
    }

    #[tokio::test]
    async fn test_process_info_sender_clone() {
        let counter = Counter {
            count: 0,
            stopped: false,
            reply_to: None,
        };

        let info = counter.spawn().await;
        let sender = info.sender();

        // Both senders should work
        let _ = sender.send(CounterMsg::Stop);
    }

    #[tokio::test]
    async fn test_process_send_messages() {
        let counter = Counter {
            count: 0,
            stopped: false,
            reply_to: None,
        };

        let info = counter.spawn().await;

        // Send increment messages
        send(&info.tx, CounterMsg::Increment);
        send(&info.tx, CounterMsg::Increment);
        send(&info.tx, CounterMsg::Stop);

        // Wait for the process to finish
        info.handle.await.unwrap();
    }

    // Echo process that echoes back messages
    struct Echo {
        stopped: bool,
    }

    impl Process<String> for Echo {
        fn should_stop(&self) -> bool {
            self.stopped
        }

        fn handle(
            &mut self,
            message: String,
            tx: &mpsc::Sender<String>,
        ) -> impl Future<Output = String> + Send {
            async move {
                if message == "STOP" {
                    self.stopped = true;
                } else {
                    // Echo back
                    let _ = tx.send(message.clone());
                }
                message
            }
        }
    }

    #[tokio::test]
    async fn test_send_function() {
        // Create a simple process to test send
        let echo = Echo { stopped: false };
        let info = echo.spawn().await;

        // Send via the send function
        send(&info.tx, "test1".to_string());
        send(&info.tx, "STOP".to_string());

        // Wait for process to finish
        info.handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_echo_process() {
        let echo = Echo { stopped: false };
        let info = echo.spawn().await;

        send(&info.tx, "hello".to_string());
        send(&info.tx, "STOP".to_string());

        info.handle.await.unwrap();
    }

    // Process with initialization
    struct InitProcess {
        initialized: Arc<AtomicU32>,
        stopped: bool,
    }

    impl Process<()> for InitProcess {
        fn should_stop(&self) -> bool {
            self.stopped
        }

        fn init(&mut self, _tx: &mpsc::Sender<()>) -> impl Future<Output = ()> + Send {
            let initialized = self.initialized.clone();
            async move {
                initialized.store(1, Ordering::SeqCst);
            }
        }

        fn handle(
            &mut self,
            _message: (),
            _tx: &mpsc::Sender<()>,
        ) -> impl Future<Output = ()> + Send {
            async {
                self.stopped = true;
            }
        }
    }

    #[tokio::test]
    async fn test_process_init_called() {
        let initialized = Arc::new(AtomicU32::new(0));
        let process = InitProcess {
            initialized: initialized.clone(),
            stopped: false,
        };

        let info = process.spawn().await;

        // Give time for init to be called
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(initialized.load(Ordering::SeqCst), 1);

        // Stop the process
        send(&info.tx, ());
        info.handle.await.unwrap();
    }
}
