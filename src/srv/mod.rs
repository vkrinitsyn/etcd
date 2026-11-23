use tonic::metadata::MetadataMap;

pub mod parking;
mod kv;
mod lease;
// [coord] the Lock service, on the leader
mod lock;
// [auth] the Auth service
mod auth;
pub(crate) mod watch;
mod maintenance;


// TODO add auth with client id (uuid)
#[inline]
pub(crate) fn peer(input: &MetadataMap) -> Option<String> {
        match input.get(crate::etcdpb::XPEER){
        None => None,
        Some(x) => x.to_str().map(|v| v.to_string()).ok()
    }
}