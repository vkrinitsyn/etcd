//! [coord] The Lock service (owner, 2026-11-21): a lock per name, queued on the leader
//! (`coord.rs`), first come first served. `Lock(name, lease)` answers once the lock is
//! this caller's, with its key `name/<lease hex>` - put then, with the lease: the lease
//! ending, by `LeaseRevoke` or by expiry, frees the lock. `Unlock(key)` frees it.
use crate::cluster::EtcdNode;
use crate::coord::{has, lock_key, Ticket, XFWD};
use crate::etcdpb::etcdserverpb::PutRequest;
use crate::etcdpb::v3lockpb::lock_client::LockClient;
use crate::etcdpb::v3lockpb::lock_server::Lock;
use crate::etcdpb::v3lockpb::{LockRequest, LockResponse, UnlockRequest, UnlockResponse};
use tonic::{async_trait, Request, Response, Status};

#[async_trait]
impl Lock for EtcdNode {
    /// Waits for the lock `name` on the leader. Refused: an empty name, a lease this
    /// cluster does not hold, or one that ends while waiting; a leader that stops
    /// being one while it waits answers `unavailable`. A caller that goes away leaves
    /// the queue.
    async fn lock(&self, request: Request<LockRequest>) -> Result<Response<LockResponse>, Status> {
        let name = request.get_ref().name.clone();
        self.check_call(request.metadata(), Some((&lock_key(&name, request.get_ref().lease), &[], crate::coord::WRITE))).await?;
        let (meta, _, r) = request.into_parts();
        if r.name.is_empty() {
            return Err(Status::invalid_argument(tr!("lock.no_name")));
        }
        // [embed] answered at once: granted, or refused while held or waited for
        let try_only = has(&meta, crate::embed::XTRY);
        if let Some(ch) = self.route_to_leader(has(&meta, XFWD)).await? {
            let mut call = self.peer_call(r, XFWD, Some(&meta)).await;
            if try_only {
                call.metadata_mut().insert(crate::embed::XTRY, tonic::metadata::MetadataValue::from_static("1"));
            }
            return LockClient::new(ch).lock(call).await;
        }
        let mut r = r;
        // [sys] the global lock timeout: a lock asked with no lease holds one of `lock_ttl`
        // made for it - the hex after its key's last `/` - freed when it is not kept alive.
        // [review 2026-11-21] Kept alive here while its call waits in the queue
        let implicit = if r.lease == 0 {
            let ttl = self.cfg.read().await.lock_ttl_secs() as i64;
            r.lease = self.grant_lease_here(0, ttl).await?.0;
            Some(ttl)
        } else {
            None
        };
        // etcd's rule: the same lease locking the same name again is the one acquisition
        let held = self.coord.locks.lock().unwrap_or_else(|p| p.into_inner())
            .get(&r.name).and_then(|q| q.holder.clone()).filter(|(_, l)| *l == r.lease);
        if let Some((key, _)) = held {
            return Ok(Response::new(LockResponse { header: Some(self.response_header()), key }));
        }
        let ticket = Ticket { coord: self.coord.clone(), name: r.name.clone(), id: self.coord.next_ticket() };
        {
            let mut locks = self.coord.locks.lock().unwrap_or_else(|p| p.into_inner());
            let q = locks.entry(r.name.clone()).or_default();
            if try_only && (q.holder.is_some() || !q.waiting.is_empty()) {
                drop(locks);
                return Err(Status::aborted(tr!("lock.busy")));
            }
            q.waiting.push_back(ticket.id);
        }
        loop {
            // armed before the look, so a change between the look and the wait is not missed
            let changed = self.coord.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            // [review 2026-11-21, 2] superseded meanwhile, it leads no more until it takes over again
            if !self.is_leader().await || !self.coord.was_leader.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(Status::unavailable(tr!("coord.not_leader")));
            }
            {
                let mut leases = self.coord.leases.write().await;
                let Some(l) = leases.get_mut(&r.lease) else {
                    return Err(Status::not_found(tr!("lease.not_found", id = r.lease)));
                };
                if let Some(ttl) = implicit {
                    l.deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(ttl.max(1) as u64);
                }
            }
            let key = lock_key(&r.name, r.lease);
            let granted = {
                let mut locks = self.coord.locks.lock().unwrap_or_else(|p| p.into_inner());
                let q = locks.entry(r.name.clone()).or_default();
                if q.holder.is_none() && q.waiting.front() == Some(&ticket.id) {
                    q.waiting.pop_front();
                    q.holder = Some((key.clone(), r.lease));
                    true
                } else {
                    false
                }
            };
            if granted {
                let put = PutRequest { key: key.clone(), value: vec![], lease: r.lease, prev_kv: false, ignore_value: false, ignore_lease: false };
                // [sys] the holder's record before the answer: a new leader knows the holder
                let rec = serde_json::to_string(&crate::sys::LockRec { key: crate::sys::hex(&key), lease: r.lease }).unwrap_or_default();
                let stored = match self.put_kv(put, &None).await {
                    Ok(_) => self.sys_write(&crate::sys::lock_rec_key(&r.name), Some(rec)).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = stored {
                    // [review 2026-11-21] the key may be stored here though not replicated:
                    // taken out, or a new leader would take it for a holder; and its record,
                    // which a store may hold though it failed to say so
                    let _ = self.kv_delete(key.clone()).await;
                    let _ = self.sys_write(&crate::sys::lock_rec_key(&r.name), None).await;
                    let mut locks = self.coord.locks.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(q) = locks.get_mut(&r.name)
                        && q.holder.as_ref().is_some_and(|(k, _)| *k == key) {
                            q.holder = None;
                        }
                    drop(locks);
                    self.coord.changed.notify_waiters();
                    return Err(e);
                }
                drop(ticket);
                return Ok(Response::new(LockResponse { header: Some(self.response_header()), key }));
            }
            tokio::select! {
                _ = &mut changed => {},
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {},
            }
        }
    }

    /// Frees the lock `key` holds and deletes the key, on the leader. [review 2026-11-21]
    /// The holder's record goes first: not deleted, nothing is freed and the caller is
    /// told - a new leader must not give the lock back to a holder told it was freed.
    async fn unlock(&self, request: Request<UnlockRequest>) -> Result<Response<UnlockResponse>, Status> {
        let key = request.get_ref().key.clone();
        self.check_call(request.metadata(), Some((&key, &[], crate::coord::WRITE))).await?;
        let (meta, _, r) = request.into_parts();
        if let Some(ch) = self.route_to_leader(has(&meta, XFWD)).await? {
            return LockClient::new(ch).unlock(self.peer_call(r, XFWD, Some(&meta)).await).await;
        }
        // the names `key` holds: in the queues, else by the records (a holder from before)
        let mut names: Vec<Vec<u8>> = self.coord.locks.lock().unwrap_or_else(|p| p.into_inner()).iter()
            .filter(|(_, q)| q.holder.as_ref().is_some_and(|(k, _)| *k == r.key))
            .map(|(n, _)| n.clone())
            .collect();
        if names.is_empty() {
            let held = crate::sys::read_records(self.sys.live().await.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            names = held.locks.into_iter().filter(|(_, (k, _))| *k == r.key).map(|(n, _)| n).collect();
        }
        for name in &names {
            self.sys_write(&crate::sys::lock_rec_key(name), None).await?;
        }
        {
            let mut locks = self.coord.locks.lock().unwrap_or_else(|p| p.into_inner());
            for name in &names {
                if let Some(q) = locks.get_mut(name) {
                    q.holder = None;
                }
            }
            locks.retain(|_, q| q.holder.is_some() || !q.waiting.is_empty());
        }
        let deleted = self.kv_delete(r.key.clone()).await;
        self.coord.changed.notify_waiters();
        deleted?;
        Ok(Response::new(UnlockResponse { header: Some(self.response_header()) }))
    }
}
