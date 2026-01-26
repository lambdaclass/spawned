use std::collections::HashMap;

use spawned_concurrency::{
    messages::Unused,
    threads::{Actor, ActorRef, InitResult, RequestResponse},
};

use crate::messages::{BankError, BankInMessage as InMessage, BankOutMessage as OutMessage};

type MsgResult = Result<OutMessage, BankError>;
type BankHandle = ActorRef<Bank>;

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
            .request(InMessage::Stop)
            .unwrap_or(Err(BankError::ServerError))
    }

    pub fn new_account(server: &mut BankHandle, who: String) -> MsgResult {
        server
            .request(InMessage::New { who })
            .unwrap_or(Err(BankError::ServerError))
    }

    pub fn deposit(server: &mut BankHandle, who: String, amount: i32) -> MsgResult {
        server
            .request(InMessage::Add { who, amount })
            .unwrap_or(Err(BankError::ServerError))
    }

    pub fn withdraw(server: &mut BankHandle, who: String, amount: i32) -> MsgResult {
        server
            .request(InMessage::Remove { who, amount })
            .unwrap_or(Err(BankError::ServerError))
    }
}

impl Actor for Bank {
    type Request = InMessage;
    type Message = Unused;
    type Reply = MsgResult;
    type Error = BankError;

    // Initializing "main" account with 1000 in balance to test init() callback.
    fn init(mut self, _handle: &ActorRef<Self>) -> Result<InitResult<Self>, Self::Error> {
        self.accounts.insert("main".to_string(), 1000);
        Ok(InitResult::Success(self))
    }

    fn handle_request(&mut self, message: Self::Request, _handle: &BankHandle) -> RequestResponse<Self> {
        match message.clone() {
            Self::Request::New { who } => match self.accounts.get(&who) {
                Some(_amount) => RequestResponse::Reply(Err(BankError::AlreadyACustomer { who })),
                None => {
                    self.accounts.insert(who.clone(), 0);
                    RequestResponse::Reply(Ok(OutMessage::Welcome { who }))
                }
            },
            Self::Request::Add { who, amount } => match self.accounts.get(&who) {
                Some(current) => {
                    let new_amount = current + amount;
                    self.accounts.insert(who.clone(), new_amount);
                    RequestResponse::Reply(Ok(OutMessage::Balance {
                        who,
                        amount: new_amount,
                    }))
                }
                None => RequestResponse::Reply(Err(BankError::NotACustomer { who })),
            },
            Self::Request::Remove { who, amount } => match self.accounts.get(&who) {
                Some(&current) => match current < amount {
                    true => RequestResponse::Reply(Err(BankError::InsufficientBalance {
                        who,
                        amount: current,
                    })),
                    false => {
                        let new_amount = current - amount;
                        self.accounts.insert(who.clone(), new_amount);
                        RequestResponse::Reply(Ok(OutMessage::WidrawOk {
                            who,
                            amount: new_amount,
                        }))
                    }
                },
                None => RequestResponse::Reply(Err(BankError::NotACustomer { who })),
            },
            Self::Request::Stop => RequestResponse::Stop(Ok(OutMessage::Stopped)),
        }
    }
}
