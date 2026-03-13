mod protocols;
mod server;

use protocols::{ServiceRegistryProtocol, ServiceRegistryRef, ToServiceRegistryRef};
use server::ServiceRegistry;
use spawned_concurrency::registry;
use spawned_concurrency::tasks::ActorStart as _;
use spawned_rt::tasks as rt;

fn main() {
    rt::run(async {
        // Start the service registry actor
        let svc = ServiceRegistry::new().start();

        // Register the protocol ref by name — other actors can discover it
        registry::register("service_registry", svc.to_service_registry_ref()).unwrap();

        // Register some services
        svc.register_service("web".into(), "http://localhost:8080".into())
            .await
            .unwrap();

        svc.register_service("db".into(), "postgres://localhost:5432".into())
            .await
            .unwrap();

        // A consumer discovers the registry by name (doesn't need to know ServiceRegistry type)
        let svc_ref: ServiceRegistryRef = registry::whereis("service_registry").unwrap();

        // Look up a service through the discovered ref
        let addr = svc_ref.lookup("web".into()).await.unwrap();
        tracing::info!("Looked up 'web': {:?}", addr);

        // List all registered names in the registry
        let names = registry::registered();
        tracing::info!("Registry contains: {:?}", names);

        // Direct request for all services
        let all = svc.list_all().await.unwrap();
        tracing::info!("All services: {:?}", all);

        // Clean up
        registry::unregister("service_registry");

        tracing::info!("Service discovery demo complete");
    });
}
