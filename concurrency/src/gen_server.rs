//! GernServer trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.

use spawned_rt::{self as rt, JoinHandle, Receiver, Sender};

#[derive(Debug)]
pub struct GenServerHandler<T, U> {
    pub tx: Sender<T>,
    pub handle: JoinHandle<()>,
    inner_tx: Sender<U>,
    inner_rx: Receiver<U>,
}

impl<T: Send + 'static, U: Send + 'static> GenServerHandler<GenServerMsg<T, U>, U> {
    pub fn sender(&self) -> Sender<GenServerMsg<T, U>> {
        self.tx.clone()
    }

    pub fn handle(self) -> JoinHandle<()> {
        self.handle
    }

    pub async fn rpc(
        &mut self,
        message: T,
    ) -> Option<U> {
        let _ = self.tx.send(GenServerMsg { sender: self.inner_tx.clone(), message });
        self.inner_rx.recv().await
    }
}

pub struct GenServerMsg<T, U> {
    sender: Sender<U>,
    message: T,
}

pub trait GenServer<T: Send + 'static, U: Send + 'static>
where
    Self: Send + Sync + Sized + 'static,
{   fn start() -> impl Future<Output = GenServerHandler<GenServerMsg<T, U>, U>> + Send {
        async {
            let (tx, mut rx) = rt::channel::<GenServerMsg<T, U>>();
            let (inner_tx, inner_rx) = rt::channel::<U>();
            let tx_clone = tx.clone();
            let handle = rt::spawn(async move {
                Self::init().run(&tx_clone, &mut rx).await;
            });
            GenServerHandler { tx, handle, inner_tx, inner_rx }
        }
    }

    fn run(
        &mut self,
        tx: &Sender<GenServerMsg<T, U>>,
        rx: &mut Receiver<GenServerMsg<T, U>>,
    ) -> impl Future<Output = ()> + Send {
        async {
            self.main_loop(tx, rx).await;
        }
    }

    fn main_loop(
        &mut self,
        tx: &Sender<GenServerMsg<T, U>>,
        rx: &mut Receiver<GenServerMsg<T, U>>,
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

    fn receive(
        &mut self,
        tx: &Sender<GenServerMsg<T, U>>,
        rx: &mut Receiver<GenServerMsg<T, U>>,
    ) -> impl std::future::Future<Output = ()> + Send {
        async {
            match rx.recv().await {
                Some(GenServerMsg { sender, message }) => {
                    let response = self.handle(message, tx).await;
                    let _ = &sender.send(response);
                }
                None => todo!(),
            }
        }
    }

    fn init() -> Self;

    fn handle(
        &mut self,
        message: T,
        tx: &Sender<GenServerMsg<T, U>>,
    ) -> impl Future<Output = U> + Send;
}
