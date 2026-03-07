pub trait Message: Send + 'static {
    type Result: Send + 'static;
}
