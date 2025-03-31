use tonic::metadata::MetadataMap;

pub mod parking;
mod kv;
mod lease;
mod watch;
mod maintenance;

pub const UNIMPL: &str = "Not yet";

// TODO add auth with client id (uuid)
#[inline]
fn peer(input: &MetadataMap) -> Option<String> {
        match input.get(crate::etcdpb::XPEER){
        None => None,
        Some(x) => x.to_str().map(|v| v.to_string()).ok()
    }
}