use spawned_concurrency::message::Message;

#[derive(Debug, Clone, PartialEq)]
pub enum BankOutMessage {
    Welcome { who: String },
    Balance { who: String, amount: i32 },
    WidrawOk { who: String, amount: i32 },
    Stopped,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BankError {
    AlreadyACustomer { who: String },
    NotACustomer { who: String },
    InsufficientBalance { who: String, amount: i32 },
    ServerError,
}

type MsgResult = Result<BankOutMessage, BankError>;

#[derive(Debug)]
pub struct NewAccount {
    pub who: String,
}
impl Message for NewAccount {
    type Result = MsgResult;
}

#[derive(Debug)]
pub struct Deposit {
    pub who: String,
    pub amount: i32,
}
impl Message for Deposit {
    type Result = MsgResult;
}

#[derive(Debug)]
pub struct Withdraw {
    pub who: String,
    pub amount: i32,
}
impl Message for Withdraw {
    type Result = MsgResult;
}

#[derive(Debug)]
pub struct Stop;
impl Message for Stop {
    type Result = MsgResult;
}
