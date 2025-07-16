use std::collections::HashMap;

use spawned_concurrency::{
    messages::Unused,
    tasks::{CallResponse, GenServer, GenServerHandle},
};

use crate::messages::{BankError, BankInMessage as InMessage, BankOutMessage as OutMessage};

type MsgResult = Result<OutMessage, BankError>;
type BankHandle = GenServerHandle<Bank>;
type BankState = HashMap<String, i32>;

#[derive(Default)]
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
    type CastMsg = Unused;
    type OutMsg = MsgResult;
    type Error = BankError;
    type State = BankState;

    // Initializing "main" account with 1000 in balance to test init() callback.
    async fn init(
        &mut self,
        _handle: &GenServerHandle<Self>,
        mut state: Self::State,
    ) -> Result<Self::State, Self::Error> {
        state.insert("main".to_string(), 1000);
        Ok(state)
    }

    async fn handle_call(
        &mut self,
        message: Self::CallMsg,
        _handle: &BankHandle,
        mut state: Self::State,
    ) -> CallResponse<Self> {
        match message.clone() {
            Self::CallMsg::New { who } => match state.get(&who) {
                Some(_amount) => {
                    CallResponse::Reply(state, Err(BankError::AlreadyACustomer { who }))
                }
                None => {
                    state.insert(who.clone(), 0);
                    CallResponse::Reply(state, Ok(OutMessage::Welcome { who }))
                }
            },
            Self::CallMsg::Add { who, amount } => match state.get(&who) {
                Some(current) => {
                    let new_amount = current + amount;
                    state.insert(who.clone(), new_amount);
                    CallResponse::Reply(
                        state,
                        Ok(OutMessage::Balance {
                            who,
                            amount: new_amount,
                        }),
                    )
                }
                None => CallResponse::Reply(state, Err(BankError::NotACustomer { who })),
            },
            Self::CallMsg::Remove { who, amount } => match state.get(&who) {
                Some(&current) => match current < amount {
                    true => CallResponse::Reply(
                        state,
                        Err(BankError::InsufficientBalance {
                            who,
                            amount: current,
                        }),
                    ),
                    false => {
                        let new_amount = current - amount;
                        state.insert(who.clone(), new_amount);
                        CallResponse::Reply(
                            state,
                            Ok(OutMessage::WidrawOk {
                                who,
                                amount: new_amount,
                            }),
                        )
                    }
                },
                None => CallResponse::Reply(state, Err(BankError::NotACustomer { who })),
            },
            Self::CallMsg::Stop => CallResponse::Stop(Ok(OutMessage::Stopped)),
        }
    }
}
