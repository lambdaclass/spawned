use std::collections::HashMap;

use spawned_concurrency::tasks::{Actor, Context, Handler};
use spawned_macros::actor;

use crate::protocols::service_registry_protocol::{ListAll, Lookup, RegisterService};
use crate::protocols::ServiceRegistryProtocol;

pub struct ServiceRegistry {
    services: HashMap<String, String>,
}

impl ServiceRegistry {
    pub fn new() -> Self {
        Self {
            services: HashMap::new(),
        }
    }
}

#[actor(protocol = ServiceRegistryProtocol)]
impl ServiceRegistry {
    #[request_handler]
    async fn handle_register_service(&mut self, msg: RegisterService, _ctx: &Context<Self>) {
        tracing::info!("Registered service '{}' at {}", msg.name, msg.address);
        self.services.insert(msg.name, msg.address);
    }

    #[request_handler]
    async fn handle_lookup(&mut self, msg: Lookup, _ctx: &Context<Self>) -> Option<String> {
        self.services.get(&msg.name).cloned()
    }

    #[request_handler]
    async fn handle_list_all(&mut self, _msg: ListAll, _ctx: &Context<Self>) -> Vec<(String, String)> {
        self.services.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}
