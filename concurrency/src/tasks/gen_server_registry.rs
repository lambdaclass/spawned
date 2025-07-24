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

static GENSERVER_REGISTRY: OnceCell<Arc<DashMap<String, Box<dyn AnyGenServerHandle>>>> =
    OnceCell::new();

pub struct GenServerRegistry;

impl GenServerRegistry {
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
        });
    }
}
