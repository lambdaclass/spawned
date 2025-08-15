use std::collections::HashMap;

use spawned_concurrency::{
    messages::Unused,
    tasks::{
        CallResponse, GenServer, GenServerHandle,
        InitResult::{self, Success},
    },
};

use crate::messages::{BankError, BankInMessage as InMessage, BankOutMessage as OutMessage};

type MsgResult = Result<OutMessage, BankError>;
type BankHandle = GenServerHandle<Bank>;

pub struct Bank {
    accounts: HashMap<String, i32>,
}

impl Bank {
    pub fn new() -> Self {
        Bank {
            accounts: HashMap::new(),
        }
    }
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
    type CallMsg = InMessage;
    type CastMsg = Unused;
    type OutMsg = MsgResult;
    type Error = BankError;

    // Initializing "main" account with 1000 in balance to test init() callback.
    async fn init(
        mut self,
        _handle: &GenServerHandle<Self>,
    ) -> Result<InitResult<Self>, Self::Error> {
        self.accounts.insert("main".to_string(), 1000);
        Ok(Success(self))
    }

    async fn handle_call(
        &mut self,
        message: Self::CallMsg,
        _handle: &BankHandle,
    ) -> CallResponse<Self> {
        match message.clone() {
            Self::CallMsg::New { who } => match self.accounts.get(&who) {
                Some(_amount) => CallResponse::Reply(Err(BankError::AlreadyACustomer { who })),
                None => {
                    self.accounts.insert(who.clone(), 0);
                    CallResponse::Reply(Ok(OutMessage::Welcome { who }))
                }
            },
            Self::CallMsg::Add { who, amount } => match self.accounts.get(&who) {
                Some(current) => {
                    let new_amount = current + amount;
                    self.accounts.insert(who.clone(), new_amount);
                    CallResponse::Reply(Ok(OutMessage::Balance {
                        who,
                        amount: new_amount,
                    }))
                }
                None => CallResponse::Reply(Err(BankError::NotACustomer { who })),
            },
            Self::CallMsg::Remove { who, amount } => match self.accounts.get(&who) {
                Some(&current) => match current < amount {
                    true => CallResponse::Reply(Err(BankError::InsufficientBalance {
                        who,
                        amount: current,
                    })),
                    false => {
                        let new_amount = current - amount;
                        self.accounts.insert(who.clone(), new_amount);
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
}
