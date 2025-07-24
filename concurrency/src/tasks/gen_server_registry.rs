// Inspired by ractor pid_registry:
// https://github.com/slawlor/ractor/blob/main/ractor/src/registry/pid_registry.rs

use dashmap::DashMap;
use once_cell::sync::OnceCell;
use std::sync::Arc;

use crate::tasks::{GenServer, GenServerHandle};

#[derive(Debug, thiserror::Error)]
pub enum GenServerRegistryError {
    #[error("A GenServer is already Registered at this Address")]
    AddressAlreadyTaken,
    #[error("There is no GenServer associated with this Address")]
    ServerNotFound,
}

// Wrapper trait to allow downcasting of `GenServerHandle`.
// Needed as otherwise `GenServerHandle<G>` is sized and
// thus not `dyn` compatible.
trait AnyGenServerHandle: Send + Sync {
    fn as_any(&self) -> &dyn std::any::Any;
}

impl<G: GenServer + 'static> AnyGenServerHandle for GenServerHandle<G> {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// Global registry for GenServer handles.
// This allows us to store and retrieve `GenServerHandle` instances
// by their address, similar to how PIDs work in Erlang/Elixir.
//
// We do not expose this directly, but rather through the `GenServerRegistry` struct.
static GENSERVER_REGISTRY: OnceCell<Arc<DashMap<String, Box<dyn AnyGenServerHandle>>>> =
    OnceCell::new();

/// A registry for `GenServer` instances, allowing them to be stored and retrieved by address.
/// This is similar to the PID registry in Erlang/Elixir, where processes can be registered and looked up by their identifiers.
pub struct GenServerRegistry;

impl GenServerRegistry {
    /// Adds a `GenServerHandle` to the registry under the specified address.
    /// Returns an error if the address is already taken.
    ///
    /// Example usage:
    /// ```rs
    ///     let banana_fruit_basket = FruitBasket::new("Banana", 10).start(); // FruitBasket implements GenServer
    ///     GenServerRegistry::add_entry("banana_fruit_basket", banana_fruit_basket).unwrap();
    /// ```
    pub fn add_entry<G: GenServer + 'static>(
        address: &str,
        server_handle: GenServerHandle<G>,
    ) -> Result<(), GenServerRegistryError> {
        let registry = GENSERVER_REGISTRY.get_or_init(|| Arc::new(DashMap::new()));
        if registry.contains_key(address) {
            return Err(GenServerRegistryError::AddressAlreadyTaken);
        }
        registry.insert(address.to_string(), Box::new(server_handle));
        Ok(())
    }

    /// Retrieves a `GenServerHandle` from the registry by its address.
    /// Returns an error if the address is not found or if the handle cannot be downcast
    ///
    /// Calling this function requires that the type of `GenServer` retrieved is known at compile time:
    /// ```rs
    ///     let mut retrieved_vegetable_basket: GenServerHandle<VegetableBasket> =
    ///         GenServerRegistry::get_entry("lettuce_vegetable_basket").unwrap();
    /// ```
    pub fn get_entry<G: GenServer + 'static>(
        address: &str,
    ) -> Result<GenServerHandle<G>, GenServerRegistryError> {
        let registry = GENSERVER_REGISTRY.get_or_init(|| Arc::new(DashMap::new()));
        registry
            .get(address)
            .ok_or(GenServerRegistryError::ServerNotFound)?
            .as_any()
            .downcast_ref::<GenServerHandle<G>>()
            .cloned()
            .ok_or(GenServerRegistryError::ServerNotFound)
    }

    /// Retrieves all entries of a specific `GenServer` type from the registry.
    ///
    /// Example usage:
    /// ```rs
    ///    let fruit_entries: Vec<GenServerHandle<FruitBasket>> =
    ///        GenServerRegistry::all_entries::<FruitBasket>();
    /// ```
    pub fn all_entries<G: GenServer + 'static>() -> Vec<GenServerHandle<G>> {
        let registry = GENSERVER_REGISTRY.get_or_init(|| Arc::new(DashMap::new()));
        registry
            .iter()
            .filter_map(|entry| {
                entry
                    .value()
                    .as_any()
                    .downcast_ref::<GenServerHandle<G>>()
                    .cloned()
            })
            .collect()
    }

    /// Updates an existing entry in the registry with a new `GenServerHandle`.
    /// Returns an error if the address is not found.
    pub fn update_entry<G: GenServer + 'static>(
        address: &str,
        server_handle: GenServerHandle<G>,
    ) -> Result<(), GenServerRegistryError> {
        let registry = GENSERVER_REGISTRY.get_or_init(|| Arc::new(DashMap::new()));
        if !registry.contains_key(address) {
            return Err(GenServerRegistryError::ServerNotFound);
        }
        registry.insert(address.to_string(), Box::new(server_handle));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spawned_rt::tasks::{self as rt};

    #[derive(Clone)]
    enum InMsg {
        GetCount,
    }

    #[derive(Clone)]
    enum OutMsg {
        Count(u32),
    }

    #[derive(Clone)]
    struct FruitBasket {
        _name: String,
        count: u32,
    }

    impl FruitBasket {
        pub fn new(name: &str, initial_count: u32) -> Self {
            Self {
                _name: name.to_string(),
                count: initial_count,
            }
        }
    }

    impl GenServer for FruitBasket {
        type CallMsg = InMsg;
        type CastMsg = ();
        type OutMsg = OutMsg;
        type Error = ();

        async fn handle_call(
            self,
            message: Self::CallMsg,
            _handle: &GenServerHandle<Self>,
        ) -> crate::tasks::CallResponse<Self> {
            match message {
                InMsg::GetCount => {
                    let count = self.count;
                    crate::tasks::CallResponse::Reply(self, OutMsg::Count(count))
                }
            }
        }
    }

    #[derive(Clone)]
    struct VegetableBasket {
        _name: String,
        count: u32,
    }

    impl VegetableBasket {
        pub fn new(name: &str, initial_count: u32) -> Self {
            Self {
                _name: name.to_string(),
                count: initial_count,
            }
        }
    }

    impl GenServer for VegetableBasket {
        type CallMsg = InMsg;
        type CastMsg = ();
        type OutMsg = OutMsg;
        type Error = ();

        async fn handle_call(
            self,
            message: Self::CallMsg,
            _handle: &GenServerHandle<Self>,
        ) -> crate::tasks::CallResponse<Self> {
            match message {
                InMsg::GetCount => {
                    let count = self.count;
                    crate::tasks::CallResponse::Reply(self, OutMsg::Count(count))
                }
            }
        }
    }

    // This test checks the functionality of the GenServerRegistry,
    // It's done all in a signle test function to prevent the global registry
    // state from not being reset between tests.
    #[test]
    fn test_gen_server_registry() {
        let runtime = rt::Runtime::new().unwrap();
        runtime.block_on(async move {
            let banana_fruit_basket = FruitBasket::new("Banana", 10).start();
            let lettuce_vegetable_basket = VegetableBasket::new("Lettuce", 20).start();

            // We can store different GenServer types in the registry
            assert!(GenServerRegistry::add_entry(
                "banana_fruit_basket",
                banana_fruit_basket.clone()
            )
            .is_ok());
            assert!(GenServerRegistry::add_entry(
                "lettuce_vegetable_basket",
                lettuce_vegetable_basket.clone(),
            )
            .is_ok());

            // Retrieve the FruitBasket GenServer
            let mut retrieved_fruit_basket: GenServerHandle<FruitBasket> =
                GenServerRegistry::get_entry("banana_fruit_basket").unwrap();
            let call_result = retrieved_fruit_basket.call(InMsg::GetCount).await;
            assert!(call_result.is_ok());
            if let Ok(OutMsg::Count(count)) = call_result {
                assert_eq!(count, 10);
            } else {
                panic!("Expected OutMsg::Count");
            }

            // Retrieve the VegetableBasket GenServer
            let mut retrieved_vegetable_basket: GenServerHandle<VegetableBasket> =
                GenServerRegistry::get_entry("lettuce_vegetable_basket").unwrap();

            let call_result = retrieved_vegetable_basket.call(InMsg::GetCount).await;
            assert!(call_result.is_ok());
            if let Ok(OutMsg::Count(count)) = call_result {
                assert_eq!(count, 20);
            } else {
                panic!("Expected OutMsg::Count");
            }

            // Next we check that we can retrieve all entries
            let mut amount_collected = 0;
            let fruit_entries: Vec<GenServerHandle<FruitBasket>> =
                GenServerRegistry::all_entries::<FruitBasket>();
            for mut entry in fruit_entries {
                let out_msg = entry.call(InMsg::GetCount).await.unwrap();
                let OutMsg::Count(count) = out_msg;
                amount_collected += count;
            }

            let vegetable_entries: Vec<GenServerHandle<VegetableBasket>> =
                GenServerRegistry::all_entries::<VegetableBasket>();
            for mut entry in vegetable_entries {
                let out_msg = entry.call(InMsg::GetCount).await.unwrap();
                let OutMsg::Count(count) = out_msg;
                amount_collected += count;
            }

            assert_eq!(amount_collected, 10 + 20);

            // Now we update the entry for the fruit basket
            let new_banana_fruit_basket = FruitBasket::new("Banana", 15).start();

            // We can't insert the entry directly
            assert!(GenServerRegistry::add_entry(
                "banana_fruit_basket",
                new_banana_fruit_basket.clone()
            )
            .is_err());

            // But we can update it
            assert!(GenServerRegistry::update_entry(
                "banana_fruit_basket",
                new_banana_fruit_basket
            )
            .is_ok());

            let mut updated_fruit_basket: GenServerHandle<FruitBasket> =
                GenServerRegistry::get_entry("banana_fruit_basket").unwrap();
            let updated_call_result = updated_fruit_basket.call(InMsg::GetCount).await.unwrap();
            let OutMsg::Count(count) = updated_call_result;
            assert_eq!(count, 15);

            // Finally, we check that we can't retrieve a non-existent entry
            assert!(GenServerRegistry::get_entry::<FruitBasket>("orange_fruit_basket").is_err());

            // And neither update a non-existent entry
            let orange_fruit_basket = FruitBasket::new("Orange", 5).start();
            assert!(
                GenServerRegistry::update_entry("orange_fruit_basket", orange_fruit_basket)
                    .is_err()
            );
        });
    }
}
