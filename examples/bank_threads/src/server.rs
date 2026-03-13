use std::collections::HashMap;

use spawned_concurrency::threads::{Actor, Context, Handler};
use spawned_concurrency::actor;

use crate::protocols::bank_protocol::{Deposit, NewAccount, Stop, Withdraw};
use crate::protocols::{BankError, BankOutMessage, BankProtocol, MsgResult};

pub struct Bank {
    accounts: HashMap<String, i32>,
}

#[actor(protocol = BankProtocol)]
impl Bank {
    pub fn new() -> Self {
        Bank {
            accounts: HashMap::new(),
        }
    }

    #[started]
    fn started(&mut self, _ctx: &Context<Self>) {
        self.accounts.insert("main".to_string(), 1000);
    }

    #[request_handler]
    fn handle_new_account(&mut self, msg: NewAccount, _ctx: &Context<Self>) -> MsgResult {
        match self.accounts.get(&msg.who) {
            Some(_) => Err(BankError::AlreadyACustomer { who: msg.who }),
            None => {
                self.accounts.insert(msg.who.clone(), 0);
                Ok(BankOutMessage::Welcome { who: msg.who })
            }
        }
    }

    #[request_handler]
    fn handle_deposit(&mut self, msg: Deposit, _ctx: &Context<Self>) -> MsgResult {
        match self.accounts.get(&msg.who) {
            Some(current) => {
                let new_amount = current + msg.amount;
                self.accounts.insert(msg.who.clone(), new_amount);
                Ok(BankOutMessage::Balance {
                    who: msg.who,
                    amount: new_amount,
                })
            }
            None => Err(BankError::NotACustomer { who: msg.who }),
        }
    }

    #[request_handler]
    fn handle_withdraw(&mut self, msg: Withdraw, _ctx: &Context<Self>) -> MsgResult {
        match self.accounts.get(&msg.who) {
            Some(&current) if current < msg.amount => {
                Err(BankError::InsufficientBalance {
                    who: msg.who,
                    amount: current,
                })
            }
            Some(&current) => {
                let new_amount = current - msg.amount;
                self.accounts.insert(msg.who.clone(), new_amount);
                Ok(BankOutMessage::WithdrawOk {
                    who: msg.who,
                    amount: new_amount,
                })
            }
            None => Err(BankError::NotACustomer { who: msg.who }),
        }
    }

    #[request_handler]
    fn handle_stop(&mut self, _msg: Stop, ctx: &Context<Self>) -> MsgResult {
        ctx.stop();
        Ok(BankOutMessage::Stopped)
    }
}
