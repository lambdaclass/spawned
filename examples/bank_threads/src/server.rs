use std::collections::HashMap;

use spawned_concurrency::threads::{Actor, ActorRef, Context, Handler};

use crate::messages::{BankError, BankOutMessage as OutMessage, Deposit, NewAccount, Stop, Withdraw};

type MsgResult = Result<OutMessage, BankError>;

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
    pub fn stop(server: &ActorRef<Bank>) -> MsgResult {
        server
            .send_request(Stop)
            .unwrap_or(Err(BankError::ServerError))
    }

    pub fn new_account(server: &ActorRef<Bank>, who: String) -> MsgResult {
        server
            .send_request(NewAccount { who })
            .unwrap_or(Err(BankError::ServerError))
    }

    pub fn deposit(server: &ActorRef<Bank>, who: String, amount: i32) -> MsgResult {
        server
            .send_request(Deposit { who, amount })
            .unwrap_or(Err(BankError::ServerError))
    }

    pub fn withdraw(server: &ActorRef<Bank>, who: String, amount: i32) -> MsgResult {
        server
            .send_request(Withdraw { who, amount })
            .unwrap_or(Err(BankError::ServerError))
    }
}

impl Actor for Bank {
    fn started(&mut self, _ctx: &Context<Self>) {
        self.accounts.insert("main".to_string(), 1000);
    }
}

impl Handler<NewAccount> for Bank {
    fn handle(&mut self, msg: NewAccount, _ctx: &Context<Self>) -> MsgResult {
        match self.accounts.get(&msg.who) {
            Some(_) => Err(BankError::AlreadyACustomer { who: msg.who }),
            None => {
                self.accounts.insert(msg.who.clone(), 0);
                Ok(OutMessage::Welcome { who: msg.who })
            }
        }
    }
}

impl Handler<Deposit> for Bank {
    fn handle(&mut self, msg: Deposit, _ctx: &Context<Self>) -> MsgResult {
        match self.accounts.get(&msg.who) {
            Some(current) => {
                let new_amount = current + msg.amount;
                self.accounts.insert(msg.who.clone(), new_amount);
                Ok(OutMessage::Balance {
                    who: msg.who,
                    amount: new_amount,
                })
            }
            None => Err(BankError::NotACustomer { who: msg.who }),
        }
    }
}

impl Handler<Withdraw> for Bank {
    fn handle(&mut self, msg: Withdraw, _ctx: &Context<Self>) -> MsgResult {
        match self.accounts.get(&msg.who) {
            Some(&current) => {
                if current < msg.amount {
                    Err(BankError::InsufficientBalance {
                        who: msg.who,
                        amount: current,
                    })
                } else {
                    let new_amount = current - msg.amount;
                    self.accounts.insert(msg.who.clone(), new_amount);
                    Ok(OutMessage::WidrawOk {
                        who: msg.who,
                        amount: new_amount,
                    })
                }
            }
            None => Err(BankError::NotACustomer { who: msg.who }),
        }
    }
}

impl Handler<Stop> for Bank {
    fn handle(&mut self, _msg: Stop, ctx: &Context<Self>) -> MsgResult {
        ctx.stop();
        Ok(OutMessage::Stopped)
    }
}
