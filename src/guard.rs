//! [auth] What every call is checked for (owner, 2026-11-21), before anything is done:
//!
//! - **A peer's call** - one carrying `x-peer` - carries the cluster's `peer_token` too,
//!   when one is set: `x-peer` alone is a header any client can send, and a call taken
//!   for a peer's skips the checks a client's gets. Without a token, as before.
//! - **A client's call**, with the `auth` feature and authentication on (`auth.rs`),
//!   carries a token from `Authenticate` - a signon session - and its account's roles
//!   permit what it asks: reading or writing a range of keys, or, for the management
//!   calls, the role `root`.

use tonic::metadata::MetadataMap;
use tonic::Status;

use crate::cluster::EtcdNode;

/// `a == b` in a time that does not depend on where they differ
pub(crate) fn same_secret(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// What a peer's call proves: `None` a client's call, `Some(Ok)` a peer's with the
/// cluster's token (or none set), `Some(Err)` a call naming a peer without the token
pub(crate) fn peer_proof(meta: &MetadataMap, peer_token: &str) -> Option<Result<(), Status>> {
    meta.get(crate::etcdpb::XPEER)?;
    if peer_token.is_empty() {
        return Some(Ok(()));
    }
    Some(match meta.get(crate::coord::XPEER_TOKEN) {
        Some(v) if same_secret(v.as_bytes(), peer_token.as_bytes()) => Ok(()),
        _ => Err(Status::unauthenticated(tr!("auth.bad_peer"))),
    })
}

impl EtcdNode {
    /// [auth] The checks of a call wanting `access` - a range of keys and read or write
    /// (`coord::READ`, `coord::WRITE`), or `None`: any signed-in account
    pub(crate) async fn check_call(&self, meta: &MetadataMap, access: Option<(&[u8], &[u8], i16)>) -> Result<(), Status> {
        let token = self.cfg.read().await.peer_token.clone();
        if let Some(proof) = peer_proof(meta, &token) {
            return proof;
        }
        #[cfg(feature = "auth")]
        if let Some(a) = &self.auth {
            return a.check(meta, access.map(crate::auth::Want::Keys).unwrap_or(crate::auth::Want::SignedIn)).await;
        }
        let _ = access;
        Ok(())
    }

    /// [auth] The checks of a management call: with authentication on, the role `root`
    pub(crate) async fn check_root(&self, meta: &MetadataMap) -> Result<(), Status> {
        let token = self.cfg.read().await.peer_token.clone();
        if let Some(proof) = peer_proof(meta, &token) {
            return proof;
        }
        #[cfg(feature = "auth")]
        if let Some(a) = &self.auth {
            return a.check(meta, crate::auth::Want::Root).await;
        }
        Ok(())
    }

    /// [auth] Every key range a Txn reads or writes, nested ones included: its compares
    /// and ranges read, its puts and deletes write
    pub(crate) fn txn_access(t: &crate::etcdpb::etcdserverpb::TxnRequest) -> Vec<(Vec<u8>, Vec<u8>, i16)> {
        use crate::etcdpb::etcdserverpb::request_op::Request as Op;
        let mut out: Vec<(Vec<u8>, Vec<u8>, i16)> = t.compare.iter()
            .map(|c| (c.key.clone(), c.range_end.clone(), crate::coord::READ))
            .collect();
        for op in t.success.iter().chain(t.failure.iter()) {
            match &op.request {
                Some(Op::RequestRange(r)) => out.push((r.key.clone(), r.range_end.clone(), crate::coord::READ)),
                Some(Op::RequestPut(r)) => {
                    out.push((r.key.clone(), vec![], crate::coord::WRITE));
                    // [review 2026-11-21] a previous value asked back is read
                    if r.prev_kv {
                        out.push((r.key.clone(), vec![], crate::coord::READ));
                    }
                }
                Some(Op::RequestDeleteRange(r)) => {
                    out.push((r.key.clone(), r.range_end.clone(), crate::coord::WRITE));
                    if r.prev_kv {
                        out.push((r.key.clone(), r.range_end.clone(), crate::coord::READ));
                    }
                }
                Some(Op::RequestTxn(n)) => out.extend(Self::txn_access(n)),
                None => {}
            }
        }
        out
    }
}

#[cfg(test)]
#[path = "guard_test.rs"]
mod tests;
