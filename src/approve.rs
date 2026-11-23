//! [approve] Approvals of a queue's messages (feature `approvals`): the permission counter.
//!
//! A queue with an approval definition is gated. A message put to it is held, not handed to a
//! consumer, until enough different accounts approved it. The votes are collected on a key - the
//! message's state, `/approval/<queue>/<key>`, readable and watchable as any key - and once the
//! count is reached the message goes into the queue for processing, as any other message.
//!
//! - **The definition** is the queue's and labac's: `/approval/<queue>` holds
//!   `{"need": 2, "alias": "queue"}`, put and deleted by `root`. `need` is how many different
//!   accounts approve; `alias` the labac resource kind the queue is a resource of - in signon a
//!   resource of that kind with the queue's name as its id, and ACL entries that give a group its
//!   level there (an entry on `*`: on every queue).
//! - **Who may what** is signon's approval scale (`labac::APPROVAL_ACTIONS`): view 1, comment 2,
//!   recommend 3, approve 4, veto 5, override 6 - a level gives its action and the ones below.
//!   The account is the token's, so authentication is on; its groups are its labac subject's and
//!   its signon roles.
//! - **A vote** is a put on the message's state key, its value the action: `approve`, `veto`,
//!   `override`, `recommend`, or `comment <text>`.
//!   - An approval counts once an account and is not taken back: to approve is not to disapprove.
//!   - A veto blocks every approval and drops the voting: the approvals collected are gone.
//!   - An override lifts a veto, and the voting starts again.
//!   - A recommendation and a comment are kept on the state and count for nothing.
//! - **A message is withdrawn** by a delete of its state key: by its producer, or `root`.
//! - **One node decides**, the leader, as for leases and locks: a put on another node is sent
//!   on to it. A definition and a waiting message with its votes are system records (`sys.rs`),
//!   written before the answer, so a new leader goes on with them.
//!
//! The rules themselves are plain functions here ([`vote`]); the node's side follows them.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// the namespace of the definitions and of the messages' state keys
pub const PREFIX: &str = "/approval/";
/// the labac resource kind a queue is a resource of, unless its definition names another
pub const ALIAS: &str = "queue";
/// a definition's system record: this, then the queue's name
pub(crate) const DEF_REC: &str = "apv/def/";
/// a waiting message's system record: this, the queue's name, `/`, the message key in hex
pub(crate) const MSG_REC: &str = "apv/msg/";
/// the most comments a message keeps, and the longest text of one
pub(crate) const MAX_COMMENTS: usize = 100;
pub(crate) const MAX_COMMENT: usize = 1000;

/// A queue's approval definition
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    /// how many different accounts approve a message before it goes into the queue: 1 or more
    pub need: u32,
    /// the labac resource kind the queue is a resource of
    #[serde(default = "alias_default")]
    pub alias: String,
}

fn alias_default() -> String {
    ALIAS.to_string()
}

impl Definition {
    /// a definition as its key's value says it; what is wrong with it otherwise
    pub fn parse(text: &[u8]) -> Result<Definition, String> {
        let d: Definition = serde_json::from_slice(text).map_err(|e| e.to_string())?;
        if d.need < 1 {
            return Err("need is 1 or more".to_string());
        }
        if !name_ok(&d.alias) {
            return Err("alias is a plain name: letters, digits, _ and -".to_string());
        }
        Ok(d)
    }
}

/// a queue's name, a message key or an alias as a key's one segment: not empty, no `/`
pub(crate) fn name_ok(n: &str) -> bool {
    !n.is_empty() && n.len() <= 255 && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

/// A message that waits for its approvals, with the votes collected so far
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// the key its producer put it on, as it goes into the queue once approved - hex
    pub key: String,
    /// the message - hex
    pub value: String,
    /// its producer's account
    pub by: String,
    /// the accounts that approved, each once
    #[serde(default)]
    pub approved: BTreeSet<String>,
    /// the accounts that recommended it
    #[serde(default)]
    pub recommended: BTreeSet<String>,
    /// the comments, in order: the account and its text
    #[serde(default)]
    pub comments: Vec<(String, String)>,
    /// the account whose veto blocks it
    #[serde(default)]
    pub vetoed_by: Option<String>,
}

/// What an account does with a waiting message
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Approve,
    Veto,
    Override,
    Recommend,
    Comment(String),
}

impl Action {
    /// a vote's value: the action's word, any case, and for a comment its text after it
    pub fn parse(value: &[u8]) -> Option<Action> {
        let text = String::from_utf8_lossy(value);
        let text = text.trim();
        let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        match (word.to_ascii_lowercase().as_str(), rest.trim()) {
            ("approve", "") => Some(Action::Approve),
            ("veto", "") => Some(Action::Veto),
            ("override", "") => Some(Action::Override),
            ("recommend", "") => Some(Action::Recommend),
            ("comment", t) if !t.is_empty() => Some(Action::Comment(t.chars().take(MAX_COMMENT).collect())),
            _ => None,
        }
    }

    /// its name on signon's approval scale: what the account's level is asked for
    pub fn name(&self) -> &'static str {
        match self {
            Action::Approve => "approve",
            Action::Veto => "veto",
            Action::Override => "override",
            Action::Recommend => "recommend",
            Action::Comment(_) => "comment",
        }
    }
}

/// What a vote came to
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// an approval counted: so many of the ones needed
    Counted(u32, u32),
    /// the approval that completes the count: the message goes into the queue
    Released,
    /// this account approved before: counted once
    Already(u32, u32),
    /// vetoed: no approval counts, the ones collected are dropped
    Vetoed,
    /// the veto lifted: the voting starts again
    Lifted,
    /// a recommendation or a comment, kept on the state
    Noted,
}

/// Why a vote is not taken
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// vetoed by this account: an override first
    Vetoed(String),
    /// an override of a message nobody vetoed
    NotVetoed,
}

/// [approve] The rules: `who` - an account the caller found allowed to - does `action` with the
/// waiting message `p` of a queue that needs `need` approvals
pub fn vote(p: &mut Pending, need: u32, who: &str, action: &Action) -> Result<Outcome, Refused> {
    match action {
        Action::Approve => {
            if let Some(v) = &p.vetoed_by {
                return Err(Refused::Vetoed(v.clone()));
            }
            let new = p.approved.insert(who.to_string());
            let have = p.approved.len() as u32;
            Ok(if have >= need.max(1) {
                Outcome::Released
            } else if new {
                Outcome::Counted(have, need)
            } else {
                Outcome::Already(have, need)
            })
        }
        Action::Veto => {
            p.vetoed_by = Some(who.to_string());
            p.approved.clear();
            Ok(Outcome::Vetoed)
        }
        Action::Override => {
            if p.vetoed_by.take().is_none() {
                return Err(Refused::NotVetoed);
            }
            Ok(Outcome::Lifted)
        }
        Action::Recommend => {
            p.recommended.insert(who.to_string());
            Ok(Outcome::Noted)
        }
        Action::Comment(text) => {
            if p.comments.len() >= MAX_COMMENTS {
                p.comments.remove(0);
            }
            p.comments.push((who.to_string(), text.clone()));
            Ok(Outcome::Noted)
        }
    }
}

/// What a key of the approvals' namespace names
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `/approval/<queue>`: the queue's definition
    Definition(String),
    /// `/approval/<queue>/<key>`: a waiting message's state
    Message(String, String),
}

/// the definition or the message a key of `/approval/` names; `None` no key of theirs
pub fn target(key: &[u8]) -> Option<Target> {
    let rest = std::str::from_utf8(key).ok()?.strip_prefix(PREFIX)?;
    match rest.split_once('/') {
        None if name_ok(rest) => Some(Target::Definition(rest.to_string())),
        Some((q, k)) if name_ok(q) && name_ok(k) => Some(Target::Message(q.to_string(), k.to_string())),
        _ => None,
    }
}

/// the queue and the message key of a producer's put - `/q/<queue>/p/<key>` or
/// `/queue/<queue>/producer/<key>`, the key its first segment, as the queue takes it
pub fn produced(key: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(key).ok()?;
    let (rest, mark) = match text.strip_prefix("/q/") {
        Some(r) => (r, "/p/"),
        None => (text.strip_prefix("/queue/")?, "/producer/"),
    };
    let (queue, tail) = rest.split_once(mark)?;
    let k = tail.split('/').next().unwrap_or("");
    (name_ok(queue) && name_ok(k)).then(|| (queue.to_string(), k.to_string()))
}

/// the queue a key of `/q/` or `/queue/` is of, whatever the key is there
pub fn queue_of(key: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(key).ok()?;
    let name = text.strip_prefix("/q/").or_else(|| text.strip_prefix("/queue/"))?.split('/').next().unwrap_or("");
    name_ok(name).then(|| name.to_string())
}

/// whether the range `key`..`end` (etcd's: an empty end the key alone, a zero byte every key from
/// it on) reaches into the approvals' namespace
pub fn reaches(key: &[u8], end: &[u8]) -> bool {
    let prefix = PREFIX.as_bytes();
    if end.is_empty() {
        return key.starts_with(prefix);
    }
    // the namespace is the keys from `/approval/` up to `/approval0`
    let mut after = prefix.to_vec();
    if let Some(last) = after.last_mut() {
        *last += 1;
    }
    key < after.as_slice() && (end == [0] || end > prefix)
}

/// a waiting message's state key
pub fn state_key(queue: &str, key: &str) -> Vec<u8> {
    format!("{PREFIX}{queue}/{key}").into_bytes()
}

/// a queue's definition record, and a waiting message's
pub(crate) fn def_rec(queue: &str) -> String {
    format!("{DEF_REC}{queue}")
}

pub(crate) fn msg_rec(queue: &str, key: &str) -> String {
    format!("{MSG_REC}{queue}/{}", crate::sys::hex(key.as_bytes()))
}

/// [approve] A waiting message's state as its key shows it: who put it, the count and the votes -
/// and the message itself, for the ones who decide on it
pub fn state(queue: &str, d: &Definition, p: &Pending) -> String {
    let value = crate::sys::unhex(&p.value).unwrap_or_default();
    serde_json::json!({
        "queue": queue,
        "need": d.need,
        "by": p.by,
        "approved": p.approved,
        "recommended": p.recommended,
        "comments": p.comments.iter().map(|(by, text)| serde_json::json!({ "by": by, "text": text })).collect::<Vec<_>>(),
        "vetoed_by": p.vetoed_by,
        "value": String::from_utf8_lossy(&value),
    }).to_string()
}

// ---- the node's side ---------------------------------------------------------------------

use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status};

use crate::cluster::EtcdNode;
use crate::coord::{has, XFWD};
use crate::etcdpb::etcdserverpb::kv_client::KvClient;
use crate::etcdpb::etcdserverpb::{DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse};

/// one vote, one message put, one definition at a time on a node: each reads a record, changes
/// it and writes it
static ONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

impl EtcdNode {
    /// a queue's definition, as this node holds it: `None` the queue is not gated
    pub(crate) async fn approval_of(&self, queue: &str) -> Option<Definition> {
        self.sys.get(&def_rec(queue)).await.and_then(|t| serde_json::from_str(&t).ok())
    }

    /// the caller's account: authentication is on - a vote is an account's - and its token good
    async fn approver(&self, meta: &MetadataMap) -> Result<(crate::auth::Approver, signon::labac::Acl), Status> {
        let Some(auth) = self.auth.as_ref().filter(|a| a.is_on()) else {
            return Err(Status::failed_precondition(tr!("approve.no_auth")));
        };
        let Some(token) = meta.get("token").and_then(|t| t.to_str().ok()) else {
            return Err(Status::unauthenticated(tr!("auth.no_token")));
        };
        let who = auth.approver(token).await?.ok_or_else(|| Status::unauthenticated(tr!("auth.bad_token")))?;
        Ok((who, auth.acl().await?))
    }

    /// a call of the approvals' as the leader takes it: `None` here, on the leader
    async fn approve_route(&self, meta: &MetadataMap) -> Result<Option<tonic::transport::Channel>, Status> {
        self.route_to_leader(has(meta, XFWD)).await
    }

    fn approve_ok(&self) -> Response<PutResponse> {
        Response::new(PutResponse { header: Some(self.response_header()), prev_kv: None })
    }

    /// the state a waiting message's key shows, put as an ordinary key: every node has it, and a
    /// watch of it sees each vote
    async fn approve_show(&self, queue: &str, key: &str, d: &Definition, p: &Pending) -> Result<(), Status> {
        let put = PutRequest { key: state_key(queue, key), value: state(queue, d, p).into_bytes(), ..Default::default() };
        self.put_kv(put, &None).await.map(|_| ())
    }

    /// [approve] A put that is the approvals' own: a definition (`target_only`: asked before the
    /// key's own permission, which is labac's and `root`'s here), a vote, or a message put to a
    /// gated queue. `None`: no put of theirs - it goes on as any put.
    pub(crate) async fn approve_put(&self, meta: &MetadataMap, r: &PutRequest, target_only: bool) -> Result<Option<Response<PutResponse>>, Status> {
        // a peer's own put - the state key replicated, a released message on its way to its
        // dispatcher - is no client's: only a client's call, also one sent on for it, is asked
        if meta.get(crate::etcdpb::XPEER).is_some() && !has(meta, XFWD) {
            return Ok(None);
        }
        let named = target(&r.key);
        let produced = match (&named, target_only) {
            (None, false) => produced(&r.key),
            _ => None,
        };
        let gated = match &produced {
            Some((q, _)) => self.approval_of(q).await,
            None => None,
        };
        if named.is_none() && gated.is_none() {
            if target_only {
                // a key of the namespace that names neither: nothing is kept there by hand
                return match reaches(&r.key, &[]) {
                    true => Err(Status::permission_denied(tr!("approve.not_here", key = String::from_utf8_lossy(&r.key)))),
                    false => Ok(None),
                };
            }
            // any other key of a gated queue - its registry, a delivery's - is the server's to put:
            // a client's put there would be a message nobody approved
            return match queue_of(&r.key) {
                Some(q) if self.approval_of(&q).await.is_some() => Err(Status::permission_denied(tr!("approve.not_here", key = String::from_utf8_lossy(&r.key)))),
                _ => Ok(None),
            };
        }
        if let Some(ch) = self.approve_route(meta).await? {
            return KvClient::new(ch).put(self.peer_call(r.clone(), XFWD, Some(meta)).await).await.map(Some);
        }
        let (who, acl) = self.approver(meta).await?;
        let _one = ONE.lock().await;
        match (named, produced) {
            // the queue's definition: root's to say
            (Some(Target::Definition(queue)), _) => {
                if !who.root {
                    return Err(Status::permission_denied(tr!("auth.not_root", user = who.name)));
                }
                // the server's own queues - the sequences' - are gated by nobody
                if crate::sys::reserved_queue(format!("/q/{queue}/p/x").as_bytes()) {
                    return Err(Status::invalid_argument(tr!("approve.bad_def", err = format!("{queue} is a queue of the server's own"))));
                }
                let d = Definition::parse(&r.value).map_err(|e| Status::invalid_argument(tr!("approve.bad_def", err = e)))?;
                let text = serde_json::to_string(&d).unwrap_or_default();
                self.sys_write(&def_rec(&queue), Some(text.clone())).await?;
                self.put_kv(PutRequest { key: r.key.clone(), value: text.into_bytes(), ..Default::default() }, &None).await?;
            }
            // a vote on a waiting message
            (Some(Target::Message(queue, key)), _) => {
                let none = || Status::not_found(tr!("approve.none", key = key.clone(), queue = queue.clone()));
                let d = self.approval_of(&queue).await.ok_or_else(none)?;
                let rec = msg_rec(&queue, &key);
                let mut p: Pending = self.sys.get(&rec).await.and_then(|t| serde_json::from_str(&t).ok()).ok_or_else(none)?;
                let action = Action::parse(&r.value)
                    .ok_or_else(|| Status::invalid_argument(tr!("approve.bad_action", value = String::from_utf8_lossy(&r.value).chars().take(40).collect::<String>())))?;
                if !acl.allowed(&who.subject, &d.alias, &queue, action.name()) {
                    return Err(Status::permission_denied(tr!("approve.denied", user = who.name, action = action.name(), queue = queue)));
                }
                match vote(&mut p, d.need, &who.name, &action) {
                    Err(Refused::Vetoed(by)) => return Err(Status::failed_precondition(tr!("approve.vetoed", user = by))),
                    Err(Refused::NotVetoed) => return Err(Status::failed_precondition(tr!("approve.not_vetoed"))),
                    Ok(Outcome::Released) => {
                        // into the queue first, its record after: a leader lost between the two
                        // leaves a message that is released again at its next approval - at least
                        // once, as a queue's messages are
                        let message = PutRequest { key: crate::sys::unhex(&p.key).unwrap_or_default(),
                            value: crate::sys::unhex(&p.value).unwrap_or_default(), ..Default::default() };
                        self.put_impl(Request::new(message)).await?;
                        self.sys_write(&rec, None).await?;
                        let _ = self.kv_delete(state_key(&queue, &key)).await;
                    }
                    Ok(_) => {
                        self.sys_write(&rec, Some(serde_json::to_string(&p).unwrap_or_default())).await?;
                        self.approve_show(&queue, &key, &d, &p).await?;
                    }
                }
            }
            // a message put to a gated queue: it waits
            (None, Some((queue, key))) => {
                let Some(d) = gated else { return Ok(None) };
                let rec = msg_rec(&queue, &key);
                if self.sys.get(&rec).await.is_some() {
                    return Err(Status::already_exists(tr!("approve.waits", key = key, queue = queue)));
                }
                let p = Pending { key: crate::sys::hex(&r.key), value: crate::sys::hex(&r.value), by: who.name.clone(), ..Default::default() };
                self.sys_write(&rec, Some(serde_json::to_string(&p).unwrap_or_default())).await?;
                self.approve_show(&queue, &key, &d, &p).await?;
            }
            (None, None) => return Ok(None),
        }
        Ok(Some(self.approve_ok()))
    }

    /// [approve] A delete that is the approvals' own - of one key: a waiting message withdrawn (its
    /// producer's, or `root`'s), or a queue's definition taken away (`root`'s, while no message of
    /// the queue waits). `None`: no delete of theirs.
    pub(crate) async fn approve_delete(&self, meta: &MetadataMap, r: &DeleteRangeRequest) -> Result<Option<Response<DeleteRangeResponse>>, Status> {
        if meta.get(crate::etcdpb::XPEER).is_some() && !has(meta, XFWD) {
            return Ok(None);
        }
        let Some(named) = target(&r.key).filter(|_| r.range_end.is_empty()) else {
            // a range that reaches into the namespace would take the states and leave their records -
            // while there are any: a node that gates nothing deletes as it always did
            return match reaches(&r.key, &r.range_end) && self.approvals_held().await {
                true => Err(Status::permission_denied(tr!("approve.not_here", key = String::from_utf8_lossy(&r.key)))),
                false => Ok(None),
            };
        };
        if let Some(ch) = self.approve_route(meta).await? {
            return KvClient::new(ch).delete_range(self.peer_call(r.clone(), XFWD, Some(meta)).await).await.map(Some);
        }
        let (who, _) = self.approver(meta).await?;
        let _one = ONE.lock().await;
        let deleted = match named {
            Target::Definition(queue) => {
                if !who.root {
                    return Err(Status::permission_denied(tr!("auth.not_root", user = who.name)));
                }
                let waits = format!("{MSG_REC}{queue}/");
                let n = self.sys.live().await.iter().filter(|(k, _)| k.starts_with(&waits)).count();
                if n > 0 {
                    return Err(Status::failed_precondition(tr!("approve.still_waiting", n = n, queue = queue)));
                }
                let had = self.sys.get(&def_rec(&queue)).await.is_some();
                self.sys_write(&def_rec(&queue), None).await?;
                had
            }
            Target::Message(queue, key) => {
                let rec = msg_rec(&queue, &key);
                let Some(p) = self.sys.get(&rec).await.and_then(|t| serde_json::from_str::<Pending>(&t).ok()) else {
                    return Err(Status::not_found(tr!("approve.none", key = key, queue = queue)));
                };
                if !who.root && p.by != who.name {
                    return Err(Status::permission_denied(tr!("approve.not_yours")));
                }
                self.sys_write(&rec, None).await?;
                true
            }
        };
        let _ = self.kv_delete(r.key.clone()).await;
        Ok(Some(Response::new(DeleteRangeResponse { header: Some(self.response_header()), deleted: deleted as i64, prev_kvs: vec![] })))
    }
}

impl EtcdNode {
    /// whether this node holds anything of the approvals': a definition, a waiting message
    async fn approvals_held(&self) -> bool {
        self.sys.live().await.iter().any(|(k, _)| k.starts_with(DEF_REC) || k.starts_with(MSG_REC))
    }

    /// [approve] No transaction of a client's writes the approvals' keys or into a gated queue: a
    /// definition, a vote and a message that waits are each a put of its own. `writes`: the key
    /// ranges the transaction puts or deletes (`txn_access`); what it only reads is not asked
    pub(crate) async fn approve_txn(&self, meta: &MetadataMap, writes: &[(Vec<u8>, Vec<u8>)]) -> Result<(), Status> {
        if meta.get(crate::etcdpb::XPEER).is_some() && !has(meta, XFWD) {
            return Ok(());
        }
        for (k, end) in writes {
            let gated = match queue_of(k) {
                Some(q) => self.approval_of(&q).await.is_some(),
                None => false,
            };
            if gated || (reaches(k, end) && (end.is_empty() || self.approvals_held().await)) {
                return Err(Status::permission_denied(tr!("approve.not_here", key = String::from_utf8_lossy(k))));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "approve_test.rs"]
mod tests;
