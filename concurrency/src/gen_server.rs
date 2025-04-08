//! GernServer trait and structs to create an abstraction similar to Erlang gen_server.
//! See examples/name_server for a usage example.
use std::fmt::Debug;

use spawned_rt::{self as rt, JoinHandle, mpsc, oneshot};

#[derive(Debug)]
pub struct GenServerHandle<T, U> {
    pub tx: mpsc::Sender<GenServerInMsg<T, U>>,
    #[allow(unused)]
    handle: JoinHandle<()>,
}

impl<T: Send, U: Send> GenServerHandle<T, U> {
    pub fn sender(&self) -> mpsc::Sender<GenServerInMsg<T, U>> {
        self.tx.clone()
    }

    pub async fn call(&mut self, message: T) -> U {
        let (oneshot_tx, oneshot_rx) = oneshot::channel::<U>();
        let _ = self.tx.send(GenServerInMsg::Call {
            sender: oneshot_tx,
            message,
        });
        // TODO: Should handle this error (channel has been closed; the server is down)
        oneshot_rx.await.unwrap()
    }

    pub async fn cast(&mut self, message: T) {
        let _ = self.tx.send(GenServerInMsg::Cast {
            message,
        });
    }
}

pub enum GenServerInMsg<T, U> {
    Call{
        sender: oneshot::Sender<U>,
        message: T,
    },
    Cast {
        message: T,
    },
}

pub enum CallResponse<U> {
    Reply(U),
    Stop(U),
}

pub enum CastResponse {
    NoReply,
    Stop,
}


pub trait GenServer
where
    Self: Send + Sized + 'static,
{
    type InMsg: Send + Sized;
    type OutMsg: Send + Sized;
    type State: Send;
    type Error: Debug;

    fn start() -> impl Future<Output = GenServerHandle<Self::InMsg, Self::OutMsg>> + Send {
        async {
            let (tx, mut rx) = mpsc::channel::<GenServerInMsg<Self::InMsg, Self::OutMsg>>();
            let tx_clone = tx.clone();
            let handle = rt::spawn(async move {
                Self::init().run(&tx_clone, &mut rx).await;
            });
            GenServerHandle { tx, handle }
        }
    }

    fn run(
        &mut self,
        tx: &mpsc::Sender<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
    ) -> impl Future<Output = ()> + Send {
        async {
            self.main_loop(tx, rx).await;
        }
    }

    fn main_loop(
        &mut self,
        tx: &mpsc::Sender<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
    ) -> impl Future<Output = ()> + Send {
        async {
            loop {
                if !self.receive(tx, rx).await {
                    break;
                }
            }
        }
    }

    fn receive(
        &mut self,
        tx: &mpsc::Sender<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
        rx: &mut mpsc::Receiver<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
    ) -> impl std::future::Future<Output = bool> + Send {
        async {
            match rx.recv().await {
                Some(GenServerInMsg::Call { sender, message }) => {
                    match self.handle_call(message, tx).await {
                        CallResponse::Reply(response) => {
                                                let _ = &sender.send(response);
                                                true
                                            },
                        CallResponse::Stop(response) => {
                            let _ = &sender.send(response);
                            false
                        },
                    }
                },
                Some(GenServerInMsg::Cast { message }) => {
                    match self.handle_cast(message, tx).await {
                        CastResponse::NoReply => true,
                        CastResponse::Stop => false,
                    }
                }
                None => {
                    // Channel has been closed; won't receive further messages. Stop the server.
                    false
                }
            }
        }
    }

    fn init() -> Self;


    fn handle_call(
        &mut self,
        message: Self::InMsg,
        tx: &mpsc::Sender<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
    ) -> impl Future<Output = CallResponse<Self::OutMsg>> + Send;

    fn handle_cast(
        &mut self,
        _message: Self::InMsg,
        _tx: &mpsc::Sender<GenServerInMsg<Self::InMsg, Self::OutMsg>>,
    ) -> impl Future<Output = CastResponse> + Send;
}
