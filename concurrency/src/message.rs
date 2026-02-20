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

/// Implements an existing protocol trait on a concrete type by mapping methods to messages.
///
/// ```ignore
/// protocol_impl! {
///     ChatBroadcaster for ActorRef<ChatRoom> {
///         send fn say(from: String, text: String) => Say;
///         send fn add_member(name: String, participant: ParticipantRef) => Join;
///         request fn members() -> Vec<String> => Members;
///     }
/// }
/// ```
///
/// Method kinds:
/// - `send fn` — fire-and-forget, returns `Result<(), ActorError>`
/// - `request fn` — async request via `Response<T>` (for tasks)
/// - `request sync fn` — blocking request, returns `Result<T, ActorError>` (for threads)
#[macro_export]
macro_rules! protocol_impl {
    // Entry
    ($trait_name:ident for $target:ty { $($body:tt)* }) => {
        $crate::protocol_impl!(@parse $trait_name $target [] $($body)*);
    };

    // Terminal: emit impl block
    (@parse $trait_name:ident $target:ty
        [$($impl_items:tt)*]
    ) => {
        impl $trait_name for $target {
            $($impl_items)*
        }
    };

    // send fn with params
    (@parse $trait_name:ident $target:ty
        [$($impl_items:tt)*]
        send fn $method:ident($($param:ident : $ptype:ty),+ $(,)?) => $msg:ident;
        $($rest:tt)*
    ) => {
        $crate::protocol_impl!(@parse $trait_name $target
            [$($impl_items)*
                fn $method(&self, $($param : $ptype),+) -> Result<(), $crate::error::ActorError> {
                    self.send($msg { $($param),+ })
                }
            ]
            $($rest)*
        );
    };

    // send fn without params
    (@parse $trait_name:ident $target:ty
        [$($impl_items:tt)*]
        send fn $method:ident() => $msg:ident;
        $($rest:tt)*
    ) => {
        $crate::protocol_impl!(@parse $trait_name $target
            [$($impl_items)*
                fn $method(&self) -> Result<(), $crate::error::ActorError> {
                    self.send($msg)
                }
            ]
            $($rest)*
        );
    };

    // request fn with params (async — returns Response<T>)
    (@parse $trait_name:ident $target:ty
        [$($impl_items:tt)*]
        request fn $method:ident($($param:ident : $ptype:ty),+ $(,)?) -> $ret:ty => $msg:ident;
        $($rest:tt)*
    ) => {
        $crate::protocol_impl!(@parse $trait_name $target
            [$($impl_items)*
                fn $method(&self, $($param : $ptype),+) -> $crate::tasks::Response<$ret> {
                    $crate::tasks::Response::from(self.request_raw($msg { $($param),+ }))
                }
            ]
            $($rest)*
        );
    };

    // request fn without params (async — returns Response<T>)
    (@parse $trait_name:ident $target:ty
        [$($impl_items:tt)*]
        request fn $method:ident() -> $ret:ty => $msg:ident;
        $($rest:tt)*
    ) => {
        $crate::protocol_impl!(@parse $trait_name $target
            [$($impl_items)*
                fn $method(&self) -> $crate::tasks::Response<$ret> {
                    $crate::tasks::Response::from(self.request_raw($msg))
                }
            ]
            $($rest)*
        );
    };

    // request sync fn with params (threads — returns Result<T>)
    (@parse $trait_name:ident $target:ty
        [$($impl_items:tt)*]
        request sync fn $method:ident($($param:ident : $ptype:ty),+ $(,)?) -> $ret:ty => $msg:ident;
        $($rest:tt)*
    ) => {
        $crate::protocol_impl!(@parse $trait_name $target
            [$($impl_items)*
                fn $method(&self, $($param : $ptype),+) -> Result<$ret, $crate::error::ActorError> {
                    self.request($msg { $($param),+ })
                }
            ]
            $($rest)*
        );
    };

    // request sync fn without params (threads — returns Result<T>)
    (@parse $trait_name:ident $target:ty
        [$($impl_items:tt)*]
        request sync fn $method:ident() -> $ret:ty => $msg:ident;
        $($rest:tt)*
    ) => {
        $crate::protocol_impl!(@parse $trait_name $target
            [$($impl_items)*
                fn $method(&self) -> Result<$ret, $crate::error::ActorError> {
                    self.request($msg)
                }
            ]
            $($rest)*
        );
    };
}
