use spawned_concurrency::Response;
use spawned_concurrency::protocol;

#[protocol]
pub trait ServiceRegistryProtocol: Send + Sync {
    fn register_service(&self, name: String, address: String) -> Response<()>;
    fn lookup(&self, name: String) -> Response<Option<String>>;
    fn list_all(&self) -> Response<Vec<(String, String)>>;
}
