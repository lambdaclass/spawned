//! Simple example to test concurrency/Process abstraction.
//!
//! Based on Joe's Armstrong book: Programming Erlang, Second edition
//! Section 22.1 - The Road to the Generic Server
//!
//! Erlang usage example:
//! 1> my_bank:start().
//! {ok,<0.33.0>}
//! 2> my_bank:deposit("joe", 10).
//! not_a_customer
//! 3> my_bank:new_account("joe").
//! {welcome,"joe"}
//! 4> my_bank:deposit("joe", 10).
//! {thanks,"joe",your_balance_is,10}
//! 5> my_bank:deposit("joe", 30).
//! {thanks,"joe",your_balance_is,40}
//! 6> my_bank:withdraw("joe", 15).
//! {thanks,"joe",your_balance_is,25}
//! 7> my_bank:withdraw("joe", 45).
//! {sorry,"joe",you_only_have,25,in_the_bank

mod messages;
mod server;

use std::collections::HashMap;

use messages::{BankError, BankOutMessage};
use server::Bank;
use spawned_concurrency::tasks::GenServer as _;
use spawned_rt::tasks as rt;

fn main() {
    rt::run(async {
        let mut name_server = Bank::start(HashMap::new());

        let joe = "Joe".to_string();

        let result = Bank::deposit(&mut name_server, joe.clone(), 10).await;
        tracing::info!("Deposit result {result:?}");
        assert_eq!(result, Err(BankError::NotACustomer { who: joe.clone() }));

        let result = Bank::new_account(&mut name_server, "Joe".to_string()).await;
        tracing::info!("New account result {result:?}");
        assert_eq!(result, Ok(BankOutMessage::Welcome { who: joe.clone() }));

        let result = Bank::deposit(&mut name_server, "Joe".to_string(), 10).await;
        tracing::info!("Deposit result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::Balance {
                who: joe.clone(),
                amount: 10
            })
        );

        let result = Bank::deposit(&mut name_server, "Joe".to_string(), 30).await;
        tracing::info!("Deposit result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::Balance {
                who: joe.clone(),
                amount: 40
            })
        );

        let result = Bank::withdraw(&mut name_server, "Joe".to_string(), 15).await;
        tracing::info!("Withdraw result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::WidrawOk {
                who: joe.clone(),
                amount: 25
            })
        );

        let result = Bank::withdraw(&mut name_server, "Joe".to_string(), 45).await;
        tracing::info!("Withdraw result {result:?}");
        assert_eq!(
            result,
            Err(BankError::InsufficientBalance {
                who: joe,
                amount: 25
            })
        );

        let result = Bank::stop(&mut name_server).await;
        tracing::info!("Stop result {result:?}");
        assert_eq!(result, Ok(BankOutMessage::Stopped));
    })
}
