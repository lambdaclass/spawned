//! Bank example using the new Handler<M> API.
//!
//! Based on Joe's Armstrong book: Programming Erlang, Second edition
//! Section 22.1 - The Road to the Generic Server

mod messages;
mod server;

use messages::*;
use server::Bank;
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;

fn main() {
    rt::run(async {
        let bank = Bank::new().start();

        // Testing initial balance for "main" account
        let result = bank.request(Withdraw { who: "main".into(), amount: 15 }).await.unwrap();
        tracing::info!("Withdraw result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::WithdrawOk {
                who: "main".to_string(),
                amount: 985
            })
        );

        let joe = "Joe".to_string();

        // Error on deposit for a non-existent account
        let result = bank.request(Deposit { who: joe.clone(), amount: 10 }).await.unwrap();
        tracing::info!("Deposit result {result:?}");
        assert_eq!(result, Err(BankError::NotACustomer { who: joe.clone() }));

        // Account creation
        let result = bank.request(NewAccount { who: joe.clone() }).await.unwrap();
        tracing::info!("New account result {result:?}");
        assert_eq!(result, Ok(BankOutMessage::Welcome { who: joe.clone() }));

        // Deposit
        let result = bank.request(Deposit { who: joe.clone(), amount: 10 }).await.unwrap();
        tracing::info!("Deposit result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::Balance { who: joe.clone(), amount: 10 })
        );

        // Deposit
        let result = bank.request(Deposit { who: joe.clone(), amount: 30 }).await.unwrap();
        tracing::info!("Deposit result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::Balance { who: joe.clone(), amount: 40 })
        );

        // Withdrawal
        let result = bank.request(Withdraw { who: joe.clone(), amount: 15 }).await.unwrap();
        tracing::info!("Withdraw result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::WithdrawOk { who: joe.clone(), amount: 25 })
        );

        // Withdrawal with not enough balance
        let result = bank.request(Withdraw { who: joe.clone(), amount: 45 }).await.unwrap();
        tracing::info!("Withdraw result {result:?}");
        assert_eq!(
            result,
            Err(BankError::InsufficientBalance { who: joe.clone(), amount: 25 })
        );

        // Full withdrawal
        let result = bank.request(Withdraw { who: joe.clone(), amount: 25 }).await.unwrap();
        tracing::info!("Withdraw result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::WithdrawOk { who: joe, amount: 0 })
        );

        // Stopping the bank
        let result = bank.request(Stop).await.unwrap();
        tracing::info!("Stop result {result:?}");
        assert_eq!(result, Ok(BankOutMessage::Stopped));
    })
}
