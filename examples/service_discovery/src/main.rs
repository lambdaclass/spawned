use std::collections::HashMap;
use std::time::Duration;

use spawned_concurrency::messages;
use spawned_concurrency::registry;
use spawned_concurrency::tasks::{Actor, ActorStart, Context, Handler, Recipient, request};
use spawned_macros::actor;
use spawned_rt::tasks as rt;

// --- Messages ---

messages! {
    Register { name: String, address: String } -> ();
    Lookup { name: String } -> Option<String>;
    ListAll -> Vec<(String, String)>
}

// --- ServiceRegistry actor ---

struct ServiceRegistry {
    services: HashMap<String, String>,
}

impl ServiceRegistry {
    fn new() -> Self {
        Self {
            services: HashMap::new(),
        }
    }
}

impl Actor for ServiceRegistry {}

#[actor]
impl ServiceRegistry {
    #[handler]
    async fn handle_register(&mut self, msg: Register, _ctx: &Context<Self>) {
        tracing::info!("Registered service '{}' at {}", msg.name, msg.address);
        self.services.insert(msg.name, msg.address);
    }

    #[handler]
    async fn handle_lookup(&mut self, msg: Lookup, _ctx: &Context<Self>) -> Option<String> {
        self.services.get(&msg.name).cloned()
    }

    #[handler]
    async fn handle_list_all(&mut self, _msg: ListAll, _ctx: &Context<Self>) -> Vec<(String, String)> {
        self.services.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

fn main() {
    rt::run(async {
        // Start the service registry actor
        let svc = ServiceRegistry::new().start();

        // Register it by name — other actors can discover it
        registry::register("service_registry", svc.recipient::<Lookup>()).unwrap();

        // Register some services
        svc.send(Register {
            name: "web".into(),
            address: "http://localhost:8080".into(),
        })
        .unwrap();

        svc.send(Register {
            name: "db".into(),
            address: "postgres://localhost:5432".into(),
        })
        .unwrap();

        // A consumer discovers the registry by name (doesn't need to know ServiceRegistry type)
        let lookup_recipient: Recipient<Lookup> = registry::whereis("service_registry").unwrap();

        // Look up a service
        let addr = request(
            &*lookup_recipient,
            Lookup {
                name: "web".into(),
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        tracing::info!("Looked up 'web': {:?}", addr);

        // List all registered names in the registry
        let names = registry::registered();
        tracing::info!("Registry contains: {:?}", names);

        // Direct request for all services
        let all = svc.request(ListAll).await.unwrap();
        tracing::info!("All services: {:?}", all);

        // Clean up
        registry::unregister("service_registry");

        tracing::info!("Service discovery demo complete");
    });
}
