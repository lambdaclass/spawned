use std::collections::HashMap;

use spawned_concurrency::{
    messages::Unused,
    threads::{CallResponse, GenServer, GenServerHandle},
};

use crate::messages::{BankError, BankInMessage as InMessage, BankOutMessage as OutMessage};

type MsgResult = Result<OutMessage, BankError>;
type BankHandle = GenServerHandle<Bank>;

#[derive(Clone)]
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
    type CallMsg = InMessage;
    type CastMsg = Unused;
    type OutMsg = MsgResult;
    type Error = BankError;

    // Initializing "main" account with 1000 in balance to test init() callback.
    fn init(mut self, _handle: &GenServerHandle<Self>) -> Result<Self, Self::Error> {
        self.accounts.insert("main".to_string(), 1000);
        Ok(self)
    }

    fn handle_call(mut self, message: Self::CallMsg, _handle: &BankHandle) -> CallResponse<Self> {
        match message.clone() {
            Self::CallMsg::New { who } => match self.accounts.get(&who) {
                Some(_amount) => {
                    CallResponse::Reply(self, Err(BankError::AlreadyACustomer { who }))
                }
                None => {
                    self.accounts.insert(who.clone(), 0);
                    CallResponse::Reply(self, Ok(OutMessage::Welcome { who }))
                }
            },
            Self::CallMsg::Add { who, amount } => match self.accounts.get(&who) {
                Some(current) => {
                    let new_amount = current + amount;
                    self.accounts.insert(who.clone(), new_amount);
                    CallResponse::Reply(
                        self,
                        Ok(OutMessage::Balance {
                            who,
                            amount: new_amount,
                        }),
                    )
                }
                None => CallResponse::Reply(self, Err(BankError::NotACustomer { who })),
            },
            Self::CallMsg::Remove { who, amount } => match self.accounts.get(&who) {
                Some(&current) => match current < amount {
                    true => CallResponse::Reply(
                        self,
                        Err(BankError::InsufficientBalance {
                            who,
                            amount: current,
                        }),
                    ),
                    false => {
                        let new_amount = current - amount;
                        self.accounts.insert(who.clone(), new_amount);
                        CallResponse::Reply(
                            self,
                            Ok(OutMessage::WidrawOk {
                                who,
                                amount: new_amount,
                            }),
                        )
                    }
                },
                None => CallResponse::Reply(self, Err(BankError::NotACustomer { who })),
            },
            Self::CallMsg::Stop => CallResponse::Stop(Ok(OutMessage::Stopped)),
        }
    }
}
