use std::collections::HashMap;

use spawned_concurrency::tasks::{CallResponse, CastResponse, GenServer, GenServerHandle};

use crate::messages::{BankError, BankInMessage as InMessage, BankOutMessage as OutMessage};

type MsgResult = Result<OutMessage, BankError>;
type BankHandle = GenServerHandle<Bank>;
type BankState = HashMap<String, i32>;

pub struct Bank {}

impl Bank {
    pub async fn stop(server: &mut BankHandle) -> MsgResult {
        server
            .call(InMessage::Stop)
            .await
            .unwrap_or(Err(BankError::ServerError))
    }

    pub async fn new_account(server: &mut BankHandle, who: String) -> MsgResult {
        server
            .call(InMessage::New { who })
            .await
            .unwrap_or(Err(BankError::ServerError))
    }

    pub async fn deposit(server: &mut BankHandle, who: String, amount: i32) -> MsgResult {
        server
            .call(InMessage::Add { who, amount })
            .await
            .unwrap_or(Err(BankError::ServerError))
    }

    pub async fn withdraw(server: &mut BankHandle, who: String, amount: i32) -> MsgResult {
        server
            .call(InMessage::Remove { who, amount })
            .await
            .unwrap_or(Err(BankError::ServerError))
    }
}

impl GenServer for Bank {
    type CallMsg = InMessage;
    type CastMsg = ();
    type OutMsg = MsgResult;
    type Error = BankError;
    type State = BankState;

    fn new() -> Self {
        Self {}
    }

    async fn init(
        &mut self,
        _handle: &GenServerHandle<Self>,
        _state: &mut Self::State,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn handle_call(
        &mut self,
        message: Self::CallMsg,
        _handle: &BankHandle,
        state: &mut Self::State,
    ) -> CallResponse<Self::OutMsg> {
        match message.clone() {
            Self::CallMsg::New { who } => match state.get(&who) {
                Some(_amount) => CallResponse::Reply(Err(BankError::AlreadyACustomer { who })),
                None => {
                    state.insert(who.clone(), 0);
                    CallResponse::Reply(Ok(OutMessage::Welcome { who }))
                }
            },
            Self::CallMsg::Add { who, amount } => match state.get(&who) {
                Some(current) => {
                    let new_amount = current + amount;
                    state.insert(who.clone(), new_amount);
                    CallResponse::Reply(Ok(OutMessage::Balance {
                        who,
                        amount: new_amount,
                    }))
                }
                None => CallResponse::Reply(Err(BankError::NotACustomer { who })),
            },
            Self::CallMsg::Remove { who, amount } => match state.get(&who) {
                Some(current) => match current < &amount {
                    true => CallResponse::Reply(Err(BankError::InsufficientBalance {
                        who,
                        amount: *current,
                    })),
                    false => {
                        let new_amount = current - amount;
                        state.insert(who.clone(), new_amount);
                        CallResponse::Reply(Ok(OutMessage::WidrawOk {
                            who,
                            amount: new_amount,
                        }))
                    }
                },
                None => CallResponse::Reply(Err(BankError::NotACustomer { who })),
            },
            Self::CallMsg::Stop => CallResponse::Stop(Ok(OutMessage::Stopped)),
        }
    }

    async fn handle_cast(
        &mut self,
        _message: Self::CastMsg,
        _handle: &BankHandle,
        _state: &mut Self::State,
    ) -> CastResponse {
        CastResponse::NoReply
    }
}
