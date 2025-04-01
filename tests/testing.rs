use etcd_client::{Client, Error};

pub const DEFAULT_TEST_ENDPOINT: &str = "localhost:2379";

/// the server is not implemented this feature yet 
pub const TESTING_RANGE: bool = false;

/// the server is not implemented this feature yet 
pub const TESTING_PREFIX: bool = false;

pub type Result<T> = std::result::Result<T, Error>;

/// Get client for testing.
pub async fn get_client() -> Result<Client> {
    Client::connect([DEFAULT_TEST_ENDPOINT], None).await
}
