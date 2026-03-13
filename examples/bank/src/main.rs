mod protocols;
mod server;

use protocols::{BankError, BankOutMessage, BankProtocol};
use server::Bank;
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;

fn main() {
    rt::run(async {
        let bank = Bank::new().start();

        // Testing initial balance for "main" account
        let result = bank.withdraw("main".into(), 15).await.unwrap();
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
        let result = bank.deposit(joe.clone(), 10).await.unwrap();
        tracing::info!("Deposit result {result:?}");
        assert_eq!(result, Err(BankError::NotACustomer { who: joe.clone() }));

        // Account creation
        let result = bank.new_account(joe.clone()).await.unwrap();
        tracing::info!("New account result {result:?}");
        assert_eq!(result, Ok(BankOutMessage::Welcome { who: joe.clone() }));

        // Deposit
        let result = bank.deposit(joe.clone(), 10).await.unwrap();
        tracing::info!("Deposit result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::Balance {
                who: joe.clone(),
                amount: 10
            })
        );

        // Deposit
        let result = bank.deposit(joe.clone(), 30).await.unwrap();
        tracing::info!("Deposit result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::Balance {
                who: joe.clone(),
                amount: 40
            })
        );

        // Withdrawal
        let result = bank.withdraw(joe.clone(), 15).await.unwrap();
        tracing::info!("Withdraw result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::WithdrawOk {
                who: joe.clone(),
                amount: 25
            })
        );

        // Withdrawal with not enough balance
        let result = bank.withdraw(joe.clone(), 45).await.unwrap();
        tracing::info!("Withdraw result {result:?}");
        assert_eq!(
            result,
            Err(BankError::InsufficientBalance {
                who: joe.clone(),
                amount: 25
            })
        );

        // Full withdrawal
        let result = bank.withdraw(joe.clone(), 25).await.unwrap();
        tracing::info!("Withdraw result {result:?}");
        assert_eq!(
            result,
            Ok(BankOutMessage::WithdrawOk {
                who: joe,
                amount: 0
            })
        );

        // Stopping the bank
        let result = bank.stop().await.unwrap();
        tracing::info!("Stop result {result:?}");
        assert_eq!(result, Ok(BankOutMessage::Stopped));
    })
}
