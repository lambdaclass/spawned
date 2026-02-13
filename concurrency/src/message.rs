pub trait Message: Send + 'static {
    type Result: Send + 'static;
}

/// Declarative macro for defining message types.
///
/// Supports both unit structs and structs with fields, and they can be mixed
/// in a single invocation:
///
/// ```ignore
/// messages! {
///     GetCount -> u64;
///     Deposit { who: String, amount: i32 } -> Result<u64, BankError>;
///     Stop -> ()
/// }
/// ```
#[macro_export]
macro_rules! messages {
    () => {};

    // Base: unit message
    ($(#[$meta:meta])* $name:ident -> $result:ty) => {
        $(#[$meta])*
        pub struct $name;
        impl $crate::message::Message for $name {
            type Result = $result;
        }
    };

    // Base: struct message
    ($(#[$meta:meta])* $name:ident { $($field:ident : $ftype:ty),* $(,)? } -> $result:ty) => {
        $(#[$meta])*
        pub struct $name { $(pub $field: $ftype,)* }
        impl $crate::message::Message for $name {
            type Result = $result;
        }
    };

    // Recursive: unit message followed by more
    ($(#[$meta:meta])* $name:ident -> $result:ty; $($rest:tt)*) => {
        $crate::messages!($(#[$meta])* $name -> $result);
        $crate::messages!($($rest)*);
    };

    // Recursive: struct message followed by more
    ($(#[$meta:meta])* $name:ident { $($field:ident : $ftype:ty),* $(,)? } -> $result:ty; $($rest:tt)*) => {
        $crate::messages!($(#[$meta])* $name { $($field : $ftype),* } -> $result);
        $crate::messages!($($rest)*);
    };
}

/// Fire-and-forget messages (Result type is always `()`).
///
/// ```ignore
/// send_messages! {
///     Increment;
///     Deposit { who: String, amount: i32 }
/// }
/// ```
#[macro_export]
macro_rules! send_messages {
    () => {};

    // Base: unit message
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        pub struct $name;
        impl $crate::message::Message for $name {
            type Result = ();
        }
    };

    // Base: struct message
    ($(#[$meta:meta])* $name:ident { $($field:ident : $ftype:ty),* $(,)? }) => {
        $(#[$meta])*
        pub struct $name { $(pub $field: $ftype,)* }
        impl $crate::message::Message for $name {
            type Result = ();
        }
    };

    // Recursive: unit message followed by more
    ($(#[$meta:meta])* $name:ident; $($rest:tt)*) => {
        $crate::send_messages!($(#[$meta])* $name);
        $crate::send_messages!($($rest)*);
    };

    // Recursive: struct message followed by more
    ($(#[$meta:meta])* $name:ident { $($field:ident : $ftype:ty),* $(,)? }; $($rest:tt)*) => {
        $crate::send_messages!($(#[$meta])* $name { $($field : $ftype),* });
        $crate::send_messages!($($rest)*);
    };
}

/// Request-response messages (Result type is explicitly specified).
///
/// ```ignore
/// request_messages! {
///     GetCount -> u64;
///     Lookup { key: String } -> Option<String>
/// }
/// ```
#[macro_export]
macro_rules! request_messages {
    () => {};

    // Base: unit message
    ($(#[$meta:meta])* $name:ident -> $result:ty) => {
        $(#[$meta])*
        pub struct $name;
        impl $crate::message::Message for $name {
            type Result = $result;
        }
    };

    // Base: struct message
    ($(#[$meta:meta])* $name:ident { $($field:ident : $ftype:ty),* $(,)? } -> $result:ty) => {
        $(#[$meta])*
        pub struct $name { $(pub $field: $ftype,)* }
        impl $crate::message::Message for $name {
            type Result = $result;
        }
    };

    // Recursive: unit message followed by more
    ($(#[$meta:meta])* $name:ident -> $result:ty; $($rest:tt)*) => {
        $crate::request_messages!($(#[$meta])* $name -> $result);
        $crate::request_messages!($($rest)*);
    };

    // Recursive: struct message followed by more
    ($(#[$meta:meta])* $name:ident { $($field:ident : $ftype:ty),* $(,)? } -> $result:ty; $($rest:tt)*) => {
        $crate::request_messages!($(#[$meta])* $name { $($field : $ftype),* } -> $result);
        $crate::request_messages!($($rest)*);
    };
}
