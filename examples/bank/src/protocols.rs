use spawned_concurrency::tasks::Response;
use spawned_macros::protocol;
use std::sync::Arc;

#[allow(dead_code)]
pub type BankRef = Arc<dyn BankProtocol>;
pub type MsgResult = Result<BankOutMessage, BankError>;

#[derive(Debug, Clone, PartialEq)]
pub enum BankOutMessage {
    Welcome { who: String },
    Balance { who: String, amount: i32 },
    WithdrawOk { who: String, amount: i32 },
    Stopped,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BankError {
    AlreadyACustomer { who: String },
    NotACustomer { who: String },
    InsufficientBalance { who: String, amount: i32 },
}

#[protocol]
pub trait BankProtocol: Send + Sync {
    fn new_account(&self, who: String) -> Response<MsgResult>;
    fn deposit(&self, who: String, amount: i32) -> Response<MsgResult>;
    fn withdraw(&self, who: String, amount: i32) -> Response<MsgResult>;
    fn stop(&self) -> Response<MsgResult>;
}
