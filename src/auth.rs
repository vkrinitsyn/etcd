//! [auth] The Auth API on signon's accounts (feature `auth`, owner, 2026-11-21).
//!
//! signon owns the accounts: an etcd user is a signon account, named by its address;
//! `Authenticate` is signon's sign-in, and its token a signon session (`via` `etcd`),
//! ended by signing out or by signon revoking it. Roles are signon's roles, a role's
//! permissions signon's `role_permissions` in the key space `etcd`; the role `root` may
//! do anything. Accounts are made, deleted and given passwords in signon -
//! `UserAdd`, `UserDelete` and `UserChangePassword` answer so.
//!
//! A token's account, roles and permissions are read from signon's database and kept
//! [`CACHE`] on each node: a revoked role or session stops working within that. Turning
//! authentication on or off is sent to every peer, and set at start by `auth`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::RwLock;
use tokio::time::{Duration, Instant};
use tonic::metadata::MetadataMap;
use tonic::Status;

use signon::permissions::{self, Permission};

/// how long a token's account and permissions are kept before they are read again
pub(crate) const CACHE: Duration = Duration::from_secs(5);
/// the key space of etcd's permissions in signon
pub(crate) const SCOPE: &str = "etcd";
/// the role that may do anything, as etcd's
pub(crate) const ROOT: &str = "root";

/// What a call asks for
pub(crate) enum Want<'a> {
    /// any signed-in account
    SignedIn,
    /// reading or writing (`coord::READ`, `coord::WRITE`) a range of keys
    Keys((&'a [u8], &'a [u8], i16)),
    /// the role `root`: the management calls
    Root,
}

/// A token's account as the checks need it
#[derive(Debug, Clone)]
pub(crate) struct Who {
    pub(crate) name: String,
    pub(crate) root: bool,
    pub(crate) perms: Vec<Permission>,
}

/// a token's account as read, and when: `None` for a token that names none
type Cached = (Instant, Option<Arc<Who>>);

pub(crate) struct Auth {
    pub(crate) state: signon::AccountState,
    pub(crate) enabled: AtomicBool,
    cache: RwLock<HashMap<String, Cached>>,
}

impl Auth {
    /// signon's database at `url`: its schema applied (append only), the role `root`
    /// made when missing
    pub(crate) async fn open(url: &str, enabled: bool) -> Result<Arc<Auth>, String> {
        signon::migrate(url).await.map_err(|e| tr!("auth.db", err = e))?;
        let pool = sqlx::postgres::PgPoolOptions::new().max_connections(4).connect(url).await
            .map_err(|e| tr!("auth.db", err = e))?;
        let state = signon::AccountState::from_env_with_mailer(pool, signon::Config::new("etcds", "http://localhost"), signon::Mailer::disabled())
            .await.map_err(|e| tr!("auth.db", err = e))?;
        signon::api::add_role(&state.pool, ROOT).await.map_err(|e| tr!("auth.db", err = e))?;
        Ok(Arc::new(Auth { state, enabled: AtomicBool::new(enabled), cache: RwLock::new(HashMap::new()) }))
    }

    pub(crate) fn is_on(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// forgets every token's account: after a change of roles or permissions made here
    pub(crate) async fn forget(&self) {
        self.cache.write().await.clear();
    }

    /// The account behind `token`, from the cache while it is fresh
    pub(crate) async fn who(&self, token: &str) -> Result<Option<Arc<Who>>, Status> {
        if let Some((at, w)) = self.cache.read().await.get(token)
            && at.elapsed() < CACHE {
                return Ok(w.clone());
            }
        let id = signon::api::identity(&self.state.pool, token).await
            .map_err(|e| Status::internal(tr!("auth.db", err = e)))?;
        let who = match id {
            None => None,
            Some(i) => {
                let perms = permissions::of_user(&self.state.pool, i.user_id, SCOPE).await
                    .map_err(|e| Status::internal(tr!("auth.db", err = e)))?;
                Some(Arc::new(Who {
                    name: i.email.unwrap_or_else(|| i.user_id.to_string()),
                    root: i.roles.iter().any(|r| r == ROOT),
                    perms,
                }))
            }
        };
        let mut cache = self.cache.write().await;
        cache.retain(|_, (at, _)| at.elapsed() < CACHE);
        cache.insert(token.to_string(), (Instant::now(), who.clone()));
        Ok(who)
    }

    /// The check of a client's call: nothing while authentication is off; else a live
    /// token whose account may do what the call asks
    pub(crate) async fn check(&self, meta: &MetadataMap, want: Want<'_>) -> Result<(), Status> {
        if !self.is_on() {
            return Ok(());
        }
        let Some(token) = meta.get("token").and_then(|t| t.to_str().ok()) else {
            return Err(Status::unauthenticated(tr!("auth.no_token")));
        };
        let Some(who) = self.who(token).await? else {
            return Err(Status::unauthenticated(tr!("auth.bad_token")));
        };
        if who.root {
            return Ok(());
        }
        match want {
            Want::SignedIn => Ok(()),
            Want::Keys((key, end, want)) if permissions::permits(&who.perms, key, end, want) => Ok(()),
            Want::Keys(_) => Err(Status::permission_denied(tr!("auth.denied", user = who.name))),
            Want::Root => Err(Status::permission_denied(tr!("auth.not_root", user = who.name))),
        }
    }
}

/// etcd's permission type (authpb: 0 read, 1 write, 2 both) as signon's (1, 2, 3)
pub(crate) fn from_etcd(perm_type: i32) -> Option<i16> {
    match perm_type {
        0 => Some(permissions::READ),
        1 => Some(permissions::WRITE),
        2 => Some(permissions::READWRITE),
        _ => None,
    }
}

/// signon's permission as etcd's type
pub(crate) fn to_etcd(perm: i16) -> i32 {
    match perm {
        permissions::READ => 0,
        permissions::WRITE => 1,
        _ => 2,
    }
}

/// [approve] An account as the approvals take it: its name, whether it is `root`, and its labac
/// subject - the one signon holds of it (made at a sign-in through an identity provider), its
/// signon roles among its groups: an account that only ever signed in here has those
#[cfg(feature = "approvals")]
pub(crate) struct Approver {
    pub(crate) name: String,
    pub(crate) root: bool,
    pub(crate) subject: signon::labac::Subject,
}

#[cfg(feature = "approvals")]
impl Auth {
    /// The account behind `token`, read from signon now: a vote is rare, and a role or a session
    /// taken away counts at once
    pub(crate) async fn approver(&self, token: &str) -> Result<Option<Approver>, Status> {
        let Some(i) = signon::api::identity(&self.state.pool, token).await.map_err(|e| Status::internal(tr!("auth.db", err = e)))? else {
            return Ok(None);
        };
        let mut subject = signon::labac::subject(&self.state.pool, i.user_id).await.map_err(|e| Status::internal(tr!("auth.db", err = e)))?
            .unwrap_or_else(|| signon::labac::Subject { user_id: i.user_id, ..Default::default() });
        subject.groups.extend(i.roles.iter().cloned());
        Ok(Some(Approver { name: i.email.unwrap_or_else(|| i.user_id.to_string()), root: i.roles.iter().any(|r| r == ROOT), subject }))
    }

    /// signon's resources, ACL entries and action levels, read now
    pub(crate) async fn acl(&self) -> Result<signon::labac::Acl, Status> {
        signon::labac::load_acl(&self.state.pool).await.map_err(|e| Status::internal(tr!("auth.db", err = e)))
    }
}
