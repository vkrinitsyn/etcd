use tonic::metadata::MetadataMap;

pub mod parking;
mod kv;
mod lease;
mod watch;
mod maintenance;

pub const UNIMPL: &str = "Not yet";


#[inline]
fn is_peer(input: &MetadataMap) -> bool {
    input.contains_key("x-peer")
}