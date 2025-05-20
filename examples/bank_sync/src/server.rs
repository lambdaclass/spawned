use std::collections::HashMap;

use spawned_concurrency::sync::{
    CallResponse, CastResponse, GenServer, GenServerHandle, GenServerInMsg,
};
use spawned_rt::sync::mpsc::Sender;

use crate::messages::{BankError, BankInMessage as InMessage, BankOutMessage as OutMessage};

type MsgResult = Result<OutMessage, BankError>;
type BankHandle = GenServerHandle<Bank>;
type BankHandleMessage = GenServerInMsg<Bank>;
type BankState = HashMap<String, i32>;

pub struct Bank {}

impl Bank {
    pub fn stop(server: &mut BankHandle) -> MsgResult {
        server
            .call(InMessage::Stop)
            .unwrap_or(Err(BankError::ServerError))
    }

    pub fn new_account(server: &mut BankHandle, who: String) -> MsgResult {
        server
            .call(InMessage::New { who })
            .unwrap_or(Err(BankError::ServerError))
    }

    pub fn deposit(server: &mut BankHandle, who: String, amount: i32) -> MsgResult {
        server
            .call(InMessage::Add { who, amount })
            .unwrap_or(Err(BankError::ServerError))
    }

    pub fn withdraw(server: &mut BankHandle, who: String, amount: i32) -> MsgResult {
        server
            .call(InMessage::Remove { who, amount })
            .unwrap_or(Err(BankError::ServerError))
    }
}

impl GenServer for Bank {
    type InMsg = InMessage;
    type OutMsg = MsgResult;
    type Error = BankError;
    type State = BankState;

    fn new() -> Self {
        Self {}
    }

    fn handle_call(
        &mut self,
        message: InMessage,
        _tx: &Sender<BankHandleMessage>,
        state: &mut Self::State,
    ) -> CallResponse<Self::OutMsg> {
        match message.clone() {
            InMessage::New { who } => match state.get(&who) {
                Some(_amount) => CallResponse::Reply(Err(BankError::AlreadyACustomer { who })),
                None => {
                    state.insert(who.clone(), 0);
                    CallResponse::Reply(Ok(OutMessage::Welcome { who }))
                }
            },
            InMessage::Add { who, amount } => match state.get(&who) {
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
            InMessage::Remove { who, amount } => match state.get(&who) {
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
            InMessage::Stop => CallResponse::Stop(Ok(OutMessage::Stopped)),
        }
    }

    fn handle_cast(
        &mut self,
        _message: InMessage,
        _tx: &Sender<BankHandleMessage>,
        _state: &mut Self::State,
    ) -> CastResponse {
        CastResponse::NoReply
    }
}
