//! Simple example to test concurrency/Process abstraction.
//!
//! Based on Joe's Armstrong book: Programming Erlang, Second edition
//! Section 22.1 - The Road to the Generic Server
//!
//! This example demonstrates:
//! - GenServer with Backend::Async (default, for I/O-bound work)
//! - GenServer with Backend::Thread (dedicated OS thread)
//! - GenServer with Backend::Blocking (blocking thread pool)
//! - Registry for named process lookup
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
use spawned_concurrency::{registry, Backend, GenServer as _};
use spawned_rt::tasks as rt;

fn main() {
    rt::run(async {
        // Demonstrate different backends
        println!("=== Bank Example with Different Backends ===\n");

        // Backend::Async - Best for I/O-bound work (default)
        println!("--- Testing with Backend::Async ---");
        test_bank(Backend::Async).await;

        // Backend::Thread - Dedicated OS thread for blocking work
        println!("\n--- Testing with Backend::Thread ---");
        test_bank(Backend::Thread).await;

        // Backend::Blocking - Tokio's blocking thread pool
        println!("\n--- Testing with Backend::Blocking ---");
        test_bank(Backend::Blocking).await;

        // Demonstrate registry integration
        println!("\n--- Testing Registry Integration ---");
        test_with_registry().await;
    })
}

async fn test_bank(backend: Backend) {
    // Starting the bank with specified backend
    let mut bank = Bank::new().start(backend);
    println!("  Started bank with {:?} backend", backend);

    // Testing initial balance for "main" account
    let result = Bank::withdraw(&mut bank, "main".to_string(), 15).await;
    assert_eq!(
        result,
        Ok(BankOutMessage::WidrawOk {
            who: "main".to_string(),
            amount: 985
        })
    );
    println!("  Withdrew from main account: {:?}", result);

    let joe = "Joe".to_string();

    // Error on deposit for an unexistent account
    let result = Bank::deposit(&mut bank, joe.clone(), 10).await;
    assert_eq!(result, Err(BankError::NotACustomer { who: joe.clone() }));
    println!("  Deposit to non-customer: {:?}", result);

    // Account creation
    let result = Bank::new_account(&mut bank, "Joe".to_string()).await;
    assert_eq!(result, Ok(BankOutMessage::Welcome { who: joe.clone() }));
    println!("  Created account: {:?}", result);

    // Deposit
    let result = Bank::deposit(&mut bank, "Joe".to_string(), 10).await;
    assert_eq!(
        result,
        Ok(BankOutMessage::Balance {
            who: joe.clone(),
            amount: 10
        })
    );

    // Deposit more
    let result = Bank::deposit(&mut bank, "Joe".to_string(), 30).await;
    assert_eq!(
        result,
        Ok(BankOutMessage::Balance {
            who: joe.clone(),
            amount: 40
        })
    );
    println!("  Balance after deposits: {:?}", result);

    // Withdrawal
    let result = Bank::withdraw(&mut bank, "Joe".to_string(), 15).await;
    assert_eq!(
        result,
        Ok(BankOutMessage::WidrawOk {
            who: joe.clone(),
            amount: 25
        })
    );

    // Withdrawal with not enough balance
    let result = Bank::withdraw(&mut bank, "Joe".to_string(), 45).await;
    assert_eq!(
        result,
        Err(BankError::InsufficientBalance {
            who: joe.clone(),
            amount: 25
        })
    );
    println!("  Insufficient balance error: {:?}", result);

    // Full withdrawal
    let result = Bank::withdraw(&mut bank, "Joe".to_string(), 25).await;
    assert_eq!(
        result,
        Ok(BankOutMessage::WidrawOk {
            who: joe,
            amount: 0
        })
    );

    // Stopping the bank
    let result = Bank::stop(&mut bank).await;
    assert_eq!(result, Ok(BankOutMessage::Stopped));
    println!("  Bank stopped successfully");
}

async fn test_with_registry() {
    // Start a bank and register it by name
    let bank = Bank::new().start(Backend::Async);
    let pid = spawned_concurrency::HasPid::pid(&bank);

    // Register the typed handle
    registry::register_handle("central_bank", bank).unwrap();
    println!("  Registered 'central_bank' with pid {}", pid);

    // Look up by name and use it
    if let Some(mut handle) = registry::lookup::<Bank>("central_bank") {
        let result = Bank::new_account(&mut handle, "Registry_User".to_string()).await;
        println!("  Created account via registry lookup: {:?}", result);

        let result = Bank::deposit(&mut handle, "Registry_User".to_string(), 100).await;
        println!("  Deposited via registry lookup: {:?}", result);

        let result = Bank::stop(&mut handle).await;
        println!("  Stopped via registry lookup: {:?}", result);
    }

    // Cleanup registry
    registry::unregister("central_bank");
    println!("  Unregistered 'central_bank'");
}
