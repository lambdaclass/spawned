//! GernServer trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.

use spawned_rt::{self as rt, JoinHandle, mpsc, oneshot};

#[derive(Debug)]
pub struct GenServerHandle<T, U> {
    pub tx: mpsc::Sender<GenServerMsg<T, U>>,
    #[allow(unused)]
    handle: JoinHandle<()>,
}

impl<T: Send + 'static, U: Send + 'static> GenServerHandle<T, U> {
    pub fn sender(&self) -> mpsc::Sender<GenServerMsg<T, U>> {
        self.tx.clone()
    }

    pub async fn rpc(
        &mut self,
        message: T,
    ) -> Option<U> {
        let (oneshot_tx, oneshot_rx) = oneshot::channel::<U>();
        let _ = self.tx.send(GenServerMsg { sender: oneshot_tx, message });
        oneshot_rx.await.ok()
    }
}

pub struct GenServerMsg<T, U> {
    sender: oneshot::Sender<U>,
    message: T,
}

pub trait GenServer
where
    Self: Send + Sync + Sized + 'static,
{
    type InMsg: Send + Sync + Sized + 'static;
    type OutMsg: Send + Sync + Sized + 'static;

    fn start() -> impl Future<Output = GenServerHandle<Self::InMsg, Self::OutMsg>> + Send {
        async {
            let (tx, mut rx) = mpsc::channel::<GenServerMsg<Self::InMsg, Self::OutMsg>>();
            let tx_clone = tx.clone();
            let handle = rt::spawn(async move {
                Self::init().run(&tx_clone, &mut rx).await;
            });
            GenServerHandle { tx, handle}
        }
    }

    fn run(
        &mut self,
        tx: &mpsc::Sender<GenServerMsg<Self::InMsg, Self::OutMsg>>,
        rx: &mut mpsc::Receiver<GenServerMsg<Self::InMsg, Self::OutMsg>>,
    ) -> impl Future<Output = ()> + Send {
        async {
            self.main_loop(tx, rx).await;
        }
    }

    fn main_loop(
        &mut self,
        tx: &mpsc::Sender<GenServerMsg<Self::InMsg, Self::OutMsg>>,
        rx: &mut mpsc::Receiver<GenServerMsg<Self::InMsg, Self::OutMsg>>,
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
        tx: &mpsc::Sender<GenServerMsg<Self::InMsg, Self::OutMsg>>,
        rx: &mut mpsc::Receiver<GenServerMsg<Self::InMsg, Self::OutMsg>>,
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
        message: Self::InMsg,
        tx: &mpsc::Sender<GenServerMsg<Self::InMsg, Self::OutMsg>>,
    ) -> impl Future<Output = Self::OutMsg> + Send;
}
