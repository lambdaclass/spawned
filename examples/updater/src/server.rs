<<<<<<< HEAD
use std::{collections::HashMap, time::Duration};
=======
use std::time::Duration;
>>>>>>> async_handlers

use spawned_concurrency::{
    CallResponse, CastResponse, GenServer, GenServerHandle, GenServerInMsg, send_after,
};
use spawned_rt::mpsc::Sender;

use crate::messages::{UpdaterInMessage as InMessage, UpdaterOutMessage as OutMessage};

type UpdateServerHandle = GenServerHandle<UpdaterServer>;
type UpdateServerMessage = GenServerInMsg<UpdaterServer>;
<<<<<<< HEAD
type UpdateServerState = HashMap<String, String>;

pub struct UpdaterServer {}

impl UpdaterServer {
    pub async fn check(server: &mut UpdateServerHandle, url: String) -> OutMessage {
        match server.cast(InMessage::Check(url)).await {
=======

#[derive(Clone)]
pub struct UpdateServerState {
    pub url: String,
    pub periodicity: Duration,
}
pub struct UpdaterServer {}

impl UpdaterServer {
    pub async fn check(server: &mut UpdateServerHandle) -> OutMessage {
        match server.cast(InMessage::Check).await {
>>>>>>> async_handlers
            Ok(_) => OutMessage::Ok,
            Err(_) => OutMessage::Error,
        }
    }
}

impl GenServer for UpdaterServer {
    type InMsg = InMessage;
    type OutMsg = OutMessage;
    type Error = std::fmt::Error;
    type State = UpdateServerState;

    fn new() -> Self {
        Self {}
    }

<<<<<<< HEAD
    fn initial_state(&self) -> Self::State {
        HashMap::new()
    }

    fn handle_call(
=======
    async fn handle_call(
>>>>>>> async_handlers
        &mut self,
        _message: InMessage,
        _tx: &Sender<UpdateServerMessage>,
        _state: &mut Self::State,
    ) -> CallResponse<Self::OutMsg> {
        CallResponse::Reply(OutMessage::Ok)
    }

<<<<<<< HEAD
    fn handle_cast(
        &mut self,
        message: InMessage,
        tx: &Sender<UpdateServerMessage>,
        _state: &mut Self::State,
    ) -> CastResponse {
        match message {
            Self::InMsg::Check(url) => {
                send_after(
                    Duration::from_millis(1000),
                    tx.clone(),
                    InMessage::Check("url".to_string()),
                );
                tracing::info!("Fetching: {url:?}");
                //let enter = futures::executor::enter();
                let resp = futures::executor::block_on(req());
                //drop(enter);
=======
    async fn handle_cast(
        &mut self,
        message: InMessage,
        tx: &Sender<UpdateServerMessage>,
        state: &mut Self::State,
    ) -> CastResponse {
        match message {
            Self::InMsg::Check => {
                send_after(state.periodicity, tx.clone(), InMessage::Check);
                let url = state.url.clone();
                tracing::info!("Fetching: {url}");
                let resp = req(url).await;

>>>>>>> async_handlers
                tracing::info!("Response: {resp:?}");

                CastResponse::NoReply
            }
        }
    }
}

<<<<<<< HEAD
async fn req() -> Result<String, reqwest::Error> {
    reqwest::get("https://httpbin.org/ip")
        .await?
        .text()
        .await
=======
async fn req(url: String) -> Result<String, reqwest::Error> {
    reqwest::get(url).await?.text().await
>>>>>>> async_handlers
}
