use spawned_concurrency::message::Message;

#[derive(Debug)]
pub struct NewAccount {
    pub who: String,
}
impl Message for NewAccount {
    type Result = Result<BankOutMessage, BankError>;
}

#[derive(Debug)]
pub struct Deposit {
    pub who: String,
    pub amount: i32,
}
impl Message for Deposit {
    type Result = Result<BankOutMessage, BankError>;
}

#[derive(Debug)]
pub struct Withdraw {
    pub who: String,
    pub amount: i32,
}
impl Message for Withdraw {
    type Result = Result<BankOutMessage, BankError>;
}

#[derive(Debug)]
pub struct Stop;
impl Message for Stop {
    type Result = Result<BankOutMessage, BankError>;
}

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
    ServerError,
}
