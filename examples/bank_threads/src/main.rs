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

use messages::{BankError, BankOutMessage};
use server::Bank;
use spawned_concurrency::threads::ActorStart;
use spawned_rt::threads as rt;

fn main() {
    rt::run(|| {
        // Starting the bank
        let name_server = Bank::new().start();

        // Testing initial balance for "main" account
        let result = Bank::withdraw(&name_server, "main".to_string(), 15);
        tracing::info!("Withdraw result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::WidrawOk {
                who: "main".to_string(),
                amount: 985
            })
        );

        let joe = "Joe".to_string();

        // Error on deposit for an unexistent account
        let result = Bank::deposit(&name_server, joe.clone(), 10);
        tracing::info!("Deposit result {result:?}");
        assert_eq!(result, Err(BankError::NotACustomer { who: joe.clone() }));

        // Account creation
        let result = Bank::new_account(&name_server, "Joe".to_string());
        tracing::info!("New account result {result:?}");
        assert_eq!(result, Ok(BankOutMessage::Welcome { who: joe.clone() }));

        // Deposit
        let result = Bank::deposit(&name_server, "Joe".to_string(), 10);
        tracing::info!("Deposit result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::Balance {
                who: joe.clone(),
                amount: 10
            })
        );

        // Deposit
        let result = Bank::deposit(&name_server, "Joe".to_string(), 30);
        tracing::info!("Deposit result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::Balance {
                who: joe.clone(),
                amount: 40
            })
        );

        // Withdrawal
        let result = Bank::withdraw(&name_server, "Joe".to_string(), 15);
        tracing::info!("Withdraw result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::WidrawOk {
                who: joe.clone(),
                amount: 25
            })
        );

        // Withdrawal with not enough balance
        let result = Bank::withdraw(&name_server, "Joe".to_string(), 45);
        tracing::info!("Withdraw result {result:?}");
        assert_eq!(
            result,
            Err(BankError::InsufficientBalance {
                who: joe.clone(),
                amount: 25
            })
        );

        // Full withdrawal
        let result = Bank::withdraw(&name_server, "Joe".to_string(), 25);
        tracing::info!("Withdraw result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::WidrawOk {
                who: joe,
                amount: 0
            })
        );

        // Stopping the bank
        let result = Bank::stop(&name_server);
        tracing::info!("Stop result {result:?}");
        assert_eq!(result, Ok(BankOutMessage::Stopped));
    })
}
