#[derive(Debug, Clone)]
pub enum BankInMessage {
    New { who: String },
    Add { who: String, amount: i32 },
    Remove { who: String, amount: i32 },
    Stop,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum BankOutMessage {
    Welcome { who: String },
    Balance { who: String, amount: i32 },
    WidrawOk { who: String, amount: i32 },
    Stopped,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum BankError {
    AlreadyACustomer { who: String },
    NotACustomer { who: String },
    InsufficientBalance { who: String, amount: i32 },
    ServerError,
}
