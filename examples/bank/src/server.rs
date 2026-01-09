use std::collections::HashMap;

use spawned_concurrency::{
    messages::Unused,
    tasks::{
        RequestResult, Actor, ActorRef,
        InitResult::{self, Success},
    },
};

use crate::messages::{BankError, BankInMessage as InMessage, BankOutMessage as OutMessage};

type MsgResult = Result<OutMessage, BankError>;
type BankHandle = ActorRef<Bank>;

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

impl Actor for Bank {
    type Request = InMessage;
    type Message = Unused;
    type Reply = MsgResult;
    type Error = BankError;

    // Initializing "main" account with 1000 in balance to test init() callback.
    async fn init(
        mut self,
        _handle: &ActorRef<Self>,
    ) -> Result<InitResult<Self>, Self::Error> {
        self.accounts.insert("main".to_string(), 1000);
        Ok(Success(self))
    }

    async fn handle_request(
        &mut self,
        message: Self::Request,
        _handle: &BankHandle,
    ) -> RequestResult<Self> {
        match message.clone() {
            Self::Request::New { who } => match self.accounts.get(&who) {
                Some(_amount) => RequestResult::Reply(Err(BankError::AlreadyACustomer { who })),
                None => {
                    self.accounts.insert(who.clone(), 0);
                    RequestResult::Reply(Ok(OutMessage::Welcome { who }))
                }
            },
            Self::Request::Add { who, amount } => match self.accounts.get(&who) {
                Some(current) => {
                    let new_amount = current + amount;
                    self.accounts.insert(who.clone(), new_amount);
                    RequestResult::Reply(Ok(OutMessage::Balance {
                        who,
                        amount: new_amount,
                    }))
                }
                None => RequestResult::Reply(Err(BankError::NotACustomer { who })),
            },
            Self::Request::Remove { who, amount } => match self.accounts.get(&who) {
                Some(&current) => match current < amount {
                    true => RequestResult::Reply(Err(BankError::InsufficientBalance {
                        who,
                        amount: current,
                    })),
                    false => {
                        let new_amount = current - amount;
                        self.accounts.insert(who.clone(), new_amount);
                        RequestResult::Reply(Ok(OutMessage::WidrawOk {
                            who,
                            amount: new_amount,
                        }))
                    }
                },
                None => RequestResult::Reply(Err(BankError::NotACustomer { who })),
            },
            Self::Request::Stop => RequestResult::Stop(Ok(OutMessage::Stopped)),
        }
    }
}
