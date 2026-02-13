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

/// Generates an extension trait + impl on `ActorRef<A>` for ergonomic method-call syntax.
///
/// Parameter names must match message struct field names exactly (ownership transfer).
///
/// ```ignore
/// actor_api! {
///     pub ChatRoomApi for ActorRef<ChatRoom> {
///         send fn say(from: String, text: String) => Say;
///         send fn add_member(name: String, inbox: Recipient<Deliver>) => Join;
///         request async fn members() -> Vec<String> => Members;
///     }
/// }
/// ```
///
/// For threads (sync), use `request fn` instead of `request async fn`.
#[macro_export]
macro_rules! actor_api {
    // Entry: pub trait
    (pub $trait_name:ident for $actor_ref:ty { $($body:tt)* }) => {
        $crate::actor_api!(@parse [pub] $trait_name $actor_ref [] [] $($body)*);
    };

    // Entry: private trait
    ($trait_name:ident for $actor_ref:ty { $($body:tt)* }) => {
        $crate::actor_api!(@parse [] $trait_name $actor_ref [] [] $($body)*);
    };

    // Terminal: generate trait + impl
    (@parse [$($vis:tt)*] $trait_name:ident $actor_ref:ty
        [$($trait_items:tt)*]
        [$($impl_items:tt)*]
    ) => {
        $($vis)* trait $trait_name {
            $($trait_items)*
        }
        impl $trait_name for $actor_ref {
            $($impl_items)*
        }
    };

    // send fn with params
    (@parse [$($vis:tt)*] $trait_name:ident $actor_ref:ty
        [$($trait_items:tt)*]
        [$($impl_items:tt)*]
        send fn $method:ident($($param:ident : $ptype:ty),+ $(,)?) => $msg:ident;
        $($rest:tt)*
    ) => {
        $crate::actor_api!(@parse [$($vis)*] $trait_name $actor_ref
            [$($trait_items)*
                fn $method(&self, $($param : $ptype),+) -> Result<(), $crate::error::ActorError>;
            ]
            [$($impl_items)*
                fn $method(&self, $($param : $ptype),+) -> Result<(), $crate::error::ActorError> {
                    self.send($msg { $($param),+ })
                }
            ]
            $($rest)*
        );
    };

    // send fn without params (unit message)
    (@parse [$($vis:tt)*] $trait_name:ident $actor_ref:ty
        [$($trait_items:tt)*]
        [$($impl_items:tt)*]
        send fn $method:ident() => $msg:ident;
        $($rest:tt)*
    ) => {
        $crate::actor_api!(@parse [$($vis)*] $trait_name $actor_ref
            [$($trait_items)*
                fn $method(&self) -> Result<(), $crate::error::ActorError>;
            ]
            [$($impl_items)*
                fn $method(&self) -> Result<(), $crate::error::ActorError> {
                    self.send($msg)
                }
            ]
            $($rest)*
        );
    };

    // request async fn with params
    (@parse [$($vis:tt)*] $trait_name:ident $actor_ref:ty
        [$($trait_items:tt)*]
        [$($impl_items:tt)*]
        request async fn $method:ident($($param:ident : $ptype:ty),+ $(,)?) -> $ret:ty => $msg:ident;
        $($rest:tt)*
    ) => {
        $crate::actor_api!(@parse [$($vis)*] $trait_name $actor_ref
            [$($trait_items)*
                async fn $method(&self, $($param : $ptype),+) -> Result<$ret, $crate::error::ActorError>;
            ]
            [$($impl_items)*
                async fn $method(&self, $($param : $ptype),+) -> Result<$ret, $crate::error::ActorError> {
                    self.request($msg { $($param),+ }).await
                }
            ]
            $($rest)*
        );
    };

    // request async fn without params (unit message)
    (@parse [$($vis:tt)*] $trait_name:ident $actor_ref:ty
        [$($trait_items:tt)*]
        [$($impl_items:tt)*]
        request async fn $method:ident() -> $ret:ty => $msg:ident;
        $($rest:tt)*
    ) => {
        $crate::actor_api!(@parse [$($vis)*] $trait_name $actor_ref
            [$($trait_items)*
                async fn $method(&self) -> Result<$ret, $crate::error::ActorError>;
            ]
            [$($impl_items)*
                async fn $method(&self) -> Result<$ret, $crate::error::ActorError> {
                    self.request($msg).await
                }
            ]
            $($rest)*
        );
    };

    // request fn with params (sync/threads)
    (@parse [$($vis:tt)*] $trait_name:ident $actor_ref:ty
        [$($trait_items:tt)*]
        [$($impl_items:tt)*]
        request fn $method:ident($($param:ident : $ptype:ty),+ $(,)?) -> $ret:ty => $msg:ident;
        $($rest:tt)*
    ) => {
        $crate::actor_api!(@parse [$($vis)*] $trait_name $actor_ref
            [$($trait_items)*
                fn $method(&self, $($param : $ptype),+) -> Result<$ret, $crate::error::ActorError>;
            ]
            [$($impl_items)*
                fn $method(&self, $($param : $ptype),+) -> Result<$ret, $crate::error::ActorError> {
                    self.request($msg { $($param),+ })
                }
            ]
            $($rest)*
        );
    };

    // request fn without params (sync/threads, unit message)
    (@parse [$($vis:tt)*] $trait_name:ident $actor_ref:ty
        [$($trait_items:tt)*]
        [$($impl_items:tt)*]
        request fn $method:ident() -> $ret:ty => $msg:ident;
        $($rest:tt)*
    ) => {
        $crate::actor_api!(@parse [$($vis)*] $trait_name $actor_ref
            [$($trait_items)*
                fn $method(&self) -> Result<$ret, $crate::error::ActorError>;
            ]
            [$($impl_items)*
                fn $method(&self) -> Result<$ret, $crate::error::ActorError> {
                    self.request($msg)
                }
            ]
            $($rest)*
        );
    };
}
