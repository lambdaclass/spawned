use std::collections::HashMap;

use crate::tasks::{GenServer, GenServerHandle};

#[derive(Debug, thiserror::Error)]
pub enum GenServerRegistryError {
    #[error("A GenServer is already Registered at this Address")]
    AddressAlreadyTaken,
    #[error("There is no GenServer associated with this Address")]
    ServerNotFound,
}

/// This struct represents a registry for GenServers, allowing
/// you to register, unregister, and retrieve GenServer handles
/// by their address.
///
/// Besides being useful for managing multiple GenServers,
/// it also adds the possibility of retrieving a GenServerHandle
/// globally by its address.
///
/// # Example
///
/// ```rust
/// use once_cell::sync::Lazy;
/// use spawned_concurrency::tasks::{GenServer, GenServerRegistry};
/// use spawned_rt::tasks::{self as rt};
/// use std::sync::Mutex;
///
/// #[derive(Clone)]
/// enum BankInMessage {
///     GetBalance,
/// }
///
///
/// #[derive(Clone)]
/// struct Bank {
///     balance: i32,
/// }
///
/// impl Bank {
///     pub fn new(initial_balance: i32) -> Self {
///         Bank { balance: initial_balance }
///     }
/// }
///
/// impl GenServer for Bank {
///     type CallMsg = BankInMessage;
///     type CastMsg = ();
///     type OutMsg = i32;
///     type Error = ();
///
///     async fn handle_call(
///         self,
///         message: Self::CallMsg,
///         _handle: &spawned_concurrency::tasks::GenServerHandle<Self>,
///     ) -> spawned_concurrency::tasks::CallResponse<Self> {
///         match message {
///             BankInMessage::GetBalance => {
///                 let balance = self.balance;
///                 spawned_concurrency::tasks::CallResponse::Reply(self, balance)
///             }
///         }
///     }
/// }
///
/// static GENSERVER_DIRECTORY: Lazy<Mutex<GenServerRegistry<Bank>>> =
///     Lazy::new(|| Mutex::new(GenServerRegistry::new()));
///
/// fn main() {
///     let runtime = rt::Runtime::new().unwrap();
///     runtime.block_on(async move {
///         let some_bank = Bank::new(1000).start();
///
///         GENSERVER_DIRECTORY
///             .lock()
///             .unwrap()
///             .add_entry("some_bank", some_bank.clone())
///             .unwrap();
///
///         somewhere_else().await;
///     });
/// }
///
/// async fn somewhere_else() {
///     let mut bank_handle = GENSERVER_DIRECTORY
///         .lock()
///         .unwrap()
///         .get_entry("some_bank")
///         .unwrap();
///     let balance = bank_handle.call(BankInMessage::GetBalance).await.unwrap();
///     println!("Balance: {}", balance)
/// }
/// ```
#[derive(Default)]
pub struct GenServerRegistry<G: GenServer + 'static> {
    agenda: HashMap<String, GenServerHandle<G>>,
}

impl<G: GenServer + 'static> GenServerRegistry<G> {
    /// Creates a new empty GenServer registry.
    pub fn new() -> Self {
        Self {
            agenda: HashMap::new(),
        }
    }

    /// Adds a new entry to the registry.
    /// Fails if the address is already taken.
    pub fn add_entry(
        &mut self,
        address: &str,
        server_handle: GenServerHandle<G>,
    ) -> Result<(), GenServerRegistryError> {
        if self.agenda.contains_key(address) {
            return Err(GenServerRegistryError::AddressAlreadyTaken);
        }

        self.agenda.insert(address.to_string(), server_handle);
        Ok(())
    }

    /// Removes an entry from the registry.
    /// Fails if the address does not exist.
    pub fn remove_entry(
        &mut self,
        address: &str,
    ) -> Result<GenServerHandle<G>, GenServerRegistryError> {
        self.agenda
            .remove(address)
            .ok_or(GenServerRegistryError::ServerNotFound)
    }

    /// Retrieves an entry from the registry.
    /// Fails if the address does not exist.
    pub fn get_entry(&self, address: &str) -> Result<GenServerHandle<G>, GenServerRegistryError> {
        self.agenda
            .get(address)
            .cloned()
            .ok_or(GenServerRegistryError::ServerNotFound)
    }

    /// Modifies an existing entry in the registry.
    /// If the address does not exist, it behaves like `add_entry`.
    pub fn change_entry(
        &mut self,
        address: &str,
        server_handle: GenServerHandle<G>,
    ) -> Result<(), GenServerRegistryError> {
        self.agenda.insert(address.to_string(), server_handle);
        Ok(())
    }

    /// Returns all entries in the registry as a vector.
    ///
    /// This is useful for cases where you need to call all
    /// registered GenServers.
    pub fn all_entries(&self) -> Vec<GenServerHandle<G>> {
        self.agenda.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::Lazy;
    use spawned_rt::tasks::{self as rt};
    use std::sync::Mutex;

    type AddressedGenServerHandle = GenServerHandle<AddressedGenServer>;

    #[derive(Clone)]
    struct AddressedGenServer {
        value: u8,
    }

    impl AddressedGenServer {
        pub fn new(value: u8) -> Self {
            AddressedGenServer { value }
        }
    }

    #[derive(Clone)]
    enum AddressedGenServerCallMessage {
        GetState,
    }

    impl GenServer for AddressedGenServer {
        type CallMsg = AddressedGenServerCallMessage;
        type CastMsg = ();
        type OutMsg = u8;
        type Error = ();

        async fn handle_call(
            self,
            message: Self::CallMsg,
            _handle: &GenServerHandle<Self>,
        ) -> crate::tasks::CallResponse<Self> {
            match message {
                AddressedGenServerCallMessage::GetState => {
                    let out_msg = self.value;
                    crate::tasks::CallResponse::Reply(self, out_msg)
                }
            }
        }
    }

    #[test]
    fn test_gen_server_directoy_add_entries() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            // We first instance a globally accessible GenServer directory
            static GENSERVER_DIRECTORY: Lazy<Mutex<GenServerRegistry<AddressedGenServer>>> =
                Lazy::new(|| Mutex::new(GenServerRegistry::new()));

            // We create the first server and add it to the directory
            let gen_one_handle = AddressedGenServer::new(1).start();
            assert!(GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .add_entry("SERVER_ONE", gen_one_handle.clone())
                .is_ok());

            // We create a second server and add it to the directory
            let gen_two_handle = AddressedGenServer::new(2).start();
            assert!(GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .add_entry("SERVER_TWO", gen_two_handle.clone())
                .is_ok());

            // We retrieve the first server from the directory, calling it we should retrieve its state correctly
            let mut one_address = GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .get_entry("SERVER_ONE")
                .unwrap();
            assert_eq!(
                AddressedGenServerHandle::call(
                    &mut one_address,
                    AddressedGenServerCallMessage::GetState
                )
                .await
                .unwrap(),
                1
            );

            // Same goes for the second server
            let mut two_address = GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .get_entry("SERVER_TWO")
                .unwrap();
            assert_eq!(
                AddressedGenServerHandle::call(
                    &mut two_address,
                    AddressedGenServerCallMessage::GetState
                )
                .await
                .unwrap(),
                2
            );

            // We can't retrieve a server that does not exist
            assert!(GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .get_entry("SERVER_THREE")
                .is_err());
        })
    }

    #[test]
    fn test_gen_server_directory_remove_entry() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            // We first instance a globally accessible GenServer directory
            static GENSERVER_DIRECTORY: Lazy<Mutex<GenServerRegistry<AddressedGenServer>>> =
                Lazy::new(|| Mutex::new(GenServerRegistry::new()));

            // We create the first server and add it to the directory
            let gen_one_handle = AddressedGenServer::new(1).start();
            assert!(GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .add_entry("SERVER_ONE", gen_one_handle.clone())
                .is_ok());

            // We retrieve the first server from the directory, calling it we should retrieve its state correctly
            let mut one_address = GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .get_entry("SERVER_ONE")
                .unwrap();
            assert_eq!(
                AddressedGenServerHandle::call(
                    &mut one_address,
                    AddressedGenServerCallMessage::GetState
                )
                .await
                .unwrap(),
                1
            );

            // We remove the server from the directory
            let _ = GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .remove_entry("SERVER_ONE")
                .unwrap();

            // We can no longer retrieve the server from the directory
            assert!(GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .get_entry("SERVER_ONE")
                .is_err());

            // We can still call the removed server handle, and it should return its state
            assert_eq!(
                AddressedGenServerHandle::call(
                    &mut gen_one_handle.clone(),
                    AddressedGenServerCallMessage::GetState
                )
                .await
                .unwrap(),
                1
            );

            // We can't remove a server that does not exist
            assert!(GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .remove_entry("SERVER_THREE")
                .is_err());
        });
    }

    #[test]
    fn test_gen_server_directory_modify_entry() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async {
            // We first instance a globally accessible GenServer directory
            static GENSERVER_DIRECTORY: Lazy<Mutex<GenServerRegistry<AddressedGenServer>>> =
                Lazy::new(|| Mutex::new(GenServerRegistry::new()));

            // We create the server and add it to the directory
            let gen_one_handle = AddressedGenServer::new(1).start();
            assert!(GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .add_entry("CHANGES", gen_one_handle.clone())
                .is_ok());

            // We retrieve the server from the directory, calling it we should retrieve its state correctly
            let mut retrieved_server = GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .get_entry("CHANGES")
                .unwrap();
            assert_eq!(
                AddressedGenServerHandle::call(
                    &mut retrieved_server,
                    AddressedGenServerCallMessage::GetState
                )
                .await
                .unwrap(),
                1
            );

            // We create a new server and change the entry in the directory
            let gen_two_handle = AddressedGenServer::new(2).start();
            assert!(GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .change_entry("CHANGES", gen_two_handle.clone())
                .is_ok());

            // We retrieve the second server from the directory, calling it we should retrieve its state correctly
            let mut retrieved_server = GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .get_entry("CHANGES")
                .unwrap();

            assert_eq!(
                AddressedGenServerHandle::call(
                    &mut retrieved_server,
                    AddressedGenServerCallMessage::GetState
                )
                .await
                .unwrap(),
                2
            );
        });
    }

    #[test]
    fn test_gen_server_directory_all_entries() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            // We first instance a globally accessible GenServer directory
            static GENSERVER_DIRECTORY: Lazy<Mutex<GenServerRegistry<AddressedGenServer>>> =
                Lazy::new(|| Mutex::new(GenServerRegistry::new()));

            // We create the first server and add it to the directory
            let gen_one_handle = AddressedGenServer::new(1).start();
            assert!(GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .add_entry("SERVER_ONE", gen_one_handle.clone())
                .is_ok());

            // We create a second server and add it to the directory
            let gen_two_handle = AddressedGenServer::new(2).start();
            assert!(GENSERVER_DIRECTORY
                .lock()
                .unwrap()
                .add_entry("SERVER_TWO", gen_two_handle.clone())
                .is_ok());

            // We retrieve all entries from the directory
            let all_entries = GENSERVER_DIRECTORY.lock().unwrap().all_entries();
            assert_eq!(all_entries.len(), 2);

            let mut sum = 0;
            for entry in all_entries {
                let mut server_handle = entry;
                sum += AddressedGenServerHandle::call(
                    &mut server_handle,
                    AddressedGenServerCallMessage::GetState,
                )
                .await
                .unwrap();
            }

            assert_eq!(sum, 3);
        });
    }
}
