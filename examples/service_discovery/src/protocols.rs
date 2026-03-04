use spawned_concurrency::tasks::Response;
use spawned_macros::protocol;

#[protocol]
pub trait ServiceRegistryProtocol: Send + Sync {
    fn register_service(&self, name: String, address: String) -> Response<()>;
    fn lookup(&self, name: String) -> Response<Option<String>>;
    fn list_all(&self) -> Response<Vec<(String, String)>>;
}
