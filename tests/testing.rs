use std::sync::atomic::{AtomicU16, Ordering};
use etcd_client::{Client, Error};
use etcds::cluster::EtcdNode;

// [range] Both were `false`: `get_impl` ignored `range_end` entirely, so every
// range and prefix read fell through to an exact-key lookup, missed, and
// returned `count: 0`. These assertions could not pass, and switching them off
// is how that stayed invisible - a prefix read is the shape of service
// discovery and of the queue's consumer registry, and both silently read as
// "nothing here".
pub const TESTING_RANGE: bool = true;
pub const TESTING_PREFIX: bool = true;

pub type Result<T> = std::result::Result<T, Error>;

static PORT: AtomicU16 = AtomicU16::new(22379);

pub async fn start_server() -> (EtcdNode, Client) {
    let port = PORT.fetch_add(1, Ordering::Relaxed);
    let addr = format!("localhost:{}", port);

    let mut cfg = etcds::cli::EtcdConfig::with_defaults();
    cfg.listen_client_urls = addr.clone();

    let log = slog::Logger::root(slog::Discard, slog::o!());
    let node = EtcdNode::init(
        cfg, log,
        #[cfg(feature = "tracer")] None,
    ).await.unwrap();
    node.serve().await.unwrap();

    let endpoint = format!("http://{}", addr);
    let mut client = Client::connect([endpoint.as_str()], None).await.unwrap();
    for _ in 0..50 {
        if client.status().await.is_ok() {
            return (node, client);
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
    panic!("Test server on {} did not become ready", addr);
}
