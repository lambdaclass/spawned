use std::collections::HashMap;

use spawned_concurrency::{CallResponse, CastResponse, GenServer, GenServerHandle, GenServerInMsg};
use spawned_rt::mpsc::Sender;

use crate::messages::{BankError, BankInMessage as InMessage, BankOutMessage as OutMessage};

type MsgResult = Result<OutMessage, BankError>;
type BankHandle = GenServerHandle<InMessage, MsgResult>;
type BankHandleMessage = GenServerInMsg<InMessage, MsgResult>;
type BankState = HashMap<String, i32>;

pub struct Bank {
    state: BankState,
}

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
    type InMsg = InMessage;
    type OutMsg = MsgResult;
    type Error = BankError;
    type State = BankState;

    fn init() -> Self {
        Self {
            state: HashMap::new(),
        }
    }

    fn state(&self) -> Self::State {
        self.state.clone()
    }

    fn set_state(&mut self, state: Self::State) {
        self.state = state;
    }

    fn handle_call(
        &mut self,
        message: InMessage,
        _tx: &Sender<BankHandleMessage>,
    ) -> CallResponse<Self::OutMsg> {
        match message.clone() {
            InMessage::New { who } => match self.state.get(&who) {
                Some(_amount) => CallResponse::Reply(Err(BankError::AlreadyACustomer { who })),
                None => {
                    self.state.insert(who.clone(), 0);
                    CallResponse::Reply(Ok(OutMessage::Welcome { who }))
                }
            },
            InMessage::Add { who, amount } => match self.state.get(&who) {
                Some(current) => {
                    let new_amount = current + amount;
                    self.state.insert(who.clone(), new_amount);
                    CallResponse::Reply(Ok(OutMessage::Balance {
                        who,
                        amount: new_amount,
                    }))
                }
                None => CallResponse::Reply(Err(BankError::NotACustomer { who })),
            },
            InMessage::Remove { who, amount } => match self.state.get(&who) {
                Some(current) => match current < &amount {
                    true => {
                        CallResponse::Reply(Err(BankError::InsufficientBalance { who, amount }))
                    }
                    false => {
                        let new_amount = current - amount;
                        self.state.insert(who.clone(), new_amount);
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
    ) -> CastResponse {
        CastResponse::NoReply
    }
}
