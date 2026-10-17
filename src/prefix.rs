//! Key-prefix policy: which keys stay on this node, and which are served only
//! to a local caller.
//!
//! Both rules are **properties of the key**, deliberately, not of the caller.
//! `srv::peer` decides "did this come from a peer" by reading the `XPEER`
//! metadata header, which any client can set — so a policy phrased in terms of
//! peers would be enforced by the caller rather than by us. A prefix cannot be
//! spoofed.
//!
//! ## Two independent policies
//!
//! * **`local`** — a matching key is never broadcast. It exists on the node
//!   that wrote it and nowhere else.
//! * **`local_only`** — a matching key is answered only on a loopback
//!   connection, and refused over the network.
//!
//! They are separate because they answer different questions, and a key can
//! need one without the other: a value may legitimately propagate while still
//! being none of a remote client's business.
//!
//! ## Exceptions, and why they are needed
//!
//! A caller usually wants "this subtree is local, **except** these branches".
//! Expressing that as an allow-list of the propagating branches would break the
//! moment a new branch is added — the default would be "propagates", which is
//! the wrong way round for anything confidential. So `local` is the subtree and
//! `local_except` carves out what does travel, and a typo in a branch name
//! fails safe: it stays local.

/// Comma-separated prefix lists, parsed once at configure time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrefixPolicy {
    /// keys under these never propagate
    local: Vec<String>,
    /// ...except keys under these, which do
    local_except: Vec<String>,
    /// keys under these are served only to a loopback caller
    local_only: Vec<String>,
    /// ...except keys under these, which are served to anyone (D15: the branches
    /// meant to travel, so a peer's forwarded write of one is not refused - E1)
    local_only_except: Vec<String>,
    /// keys under these propagate only to peers in the same zone
    zone: Vec<String>,
    /// keys under these are written only by the embedder in-process, never through
    /// the gRPC API: a mirror the node keeps from its own source of truth (ytserv's
    /// `/secret/`), which a client must not be able to overwrite
    readonly: Vec<String>,
}

/// Is this caller on the network - an address that is not loopback? `None` (a
/// unix socket, an in-process call) is not.
pub fn is_remote(remote: Option<std::net::SocketAddr>) -> bool {
    remote.is_some_and(|a| !a.ip().is_loopback())
}

/// The first key after every key that starts with `p`: etcd's prefix range end.
/// `None` when there is none (`p` empty, or all `0xff`).
fn prefix_end(p: &[u8]) -> Option<Vec<u8>> {
    let mut e = p.to_vec();
    while let Some(&last) = e.last() {
        if last == 0xff {
            e.pop();
        } else {
            *e.last_mut().unwrap() += 1;
            return Some(e);
        }
    }
    None
}

/// Might the request range `[key, end)` hold a key that starts with `p`? An empty
/// `end` is the single key `key`; `end == [0]` is every key from `key` on.
fn range_overlaps(key: &[u8], end: &[u8], p: &[u8]) -> bool {
    if end.is_empty() {
        return key.starts_with(p);
    }
    let below_hi = match prefix_end(p) {
        Some(hi) => key < hi.as_slice(),
        None => true,
    };
    let above_lo = end == [0u8] || end > p;
    below_hi && above_lo
}

/// Is every key of `[key, end)` under `p`?
fn range_within(key: &[u8], end: &[u8], p: &[u8]) -> bool {
    if !key.starts_with(p) {
        return false;
    }
    if end.is_empty() {
        return true;
    }
    match prefix_end(p) {
        Some(hi) => end != [0u8] && end <= hi.as_slice(),
        None => true,
    }
}

fn split(spec: &str) -> Vec<String> {
    spec.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

impl PrefixPolicy {
    pub fn new(local: &str, local_except: &str, local_only: &str, zone: &str) -> Self {
        PrefixPolicy {
            local: split(local),
            local_except: split(local_except),
            local_only: split(local_only),
            zone: split(zone),
            ..Default::default()
        }
    }

    /// Branches of `local_only` served to remote callers too (D15).
    pub fn with_local_only_except(mut self, spec: &str) -> Self {
        self.local_only_except = split(spec);
        self
    }

    /// Prefixes no gRPC caller may write (`writable`).
    pub fn with_readonly(mut self, spec: &str) -> Self {
        self.readonly = split(spec);
        self
    }

    /// Does a key under this prefix stay inside its zone?
    ///
    /// Separate from `local`, because the two compose: a key may propagate
    /// (not local) and still go only to peers sharing this node's zone. The
    /// caller supplies the zone of each side; this only says whether the
    /// question applies.
    pub fn is_zone_scoped(&self, key: &[u8]) -> bool {
        if self.zone.is_empty() {
            return false;
        }
        let k = String::from_utf8_lossy(key);
        self.zone.iter().any(|p| k.starts_with(p.as_str()))
    }

    /// May this key be broadcast to a peer in `peer_zone`?
    ///
    /// An empty zone on either side means "unzoned", and two unzoned nodes are
    /// in the same (unnamed) zone — which is a flat cluster, and is today's
    /// behaviour unchanged.
    pub fn propagates_to_zone(&self, key: &[u8], my_zone: &str, peer_zone: &str) -> bool {
        if !self.propagates(key) {
            return false;
        }
        if !self.is_zone_scoped(key) {
            return true;
        }
        my_zone.trim() == peer_zone.trim()
    }

    /// Nothing configured: every key propagates and every key is served.
    /// This is the default, and it is byte-for-byte today's behaviour.
    pub fn is_empty(&self) -> bool {
        self.local.is_empty() && self.local_only.is_empty() && self.zone.is_empty() && self.readonly.is_empty()
    }

    /// May this key be broadcast to peers?
    ///
    /// The exception is checked first and wins, which is what makes
    /// "`/secret/` is local except `/secret/encrypted/`" express what it looks
    /// like it expresses.
    pub fn propagates(&self, key: &[u8]) -> bool {
        if self.local.is_empty() {
            return true;
        }
        let k = String::from_utf8_lossy(key);
        if !self.local.iter().any(|p| k.starts_with(p.as_str())) {
            return true;
        }
        self.local_except.iter().any(|p| k.starts_with(p.as_str()))
    }

    /// May this key be served to a caller at `remote`?
    ///
    /// `None` means the transport could not place the caller — a unix socket,
    /// or an in-process call — and is treated as local. Refusing those would
    /// refuse ourselves.
    ///
    /// **Loopback is not identity.** On a shared host every local process is
    /// inside this boundary; what it keeps out is the network.
    pub fn may_serve(&self, key: &[u8], remote: Option<std::net::SocketAddr>) -> bool {
        if self.local_only.is_empty() {
            return true;
        }
        let k = String::from_utf8_lossy(key);
        if !self.local_only.iter().any(|p| k.starts_with(p.as_str())) {
            return true;
        }
        if self.local_only_except.iter().any(|p| k.starts_with(p.as_str())) {
            return true;
        }
        !is_remote(remote)
    }

    /// May a caller at `remote` read, watch or count the range `[key, end)`? No
    /// when it is remote and the range may hold a `local_only` key that no
    /// exception covers (E2/E3: a range or a watch used to be checked by its first
    /// key alone, so `key = "", range_end = "\0"` returned every key).
    pub fn range_may_serve(&self, key: &[u8], end: &[u8], remote: Option<std::net::SocketAddr>) -> bool {
        if self.local_only.is_empty() || !is_remote(remote) {
            return true;
        }
        if !self.local_only.iter().any(|p| range_overlaps(key, end, p.as_bytes())) {
            return true;
        }
        // it touches a local-only subtree: served only when it lies wholly inside
        // one of that subtree's exceptions
        self.local_only_except.iter().any(|e| range_within(key, end, e.as_bytes()))
    }

    /// May a gRPC caller write this key? Never under a `readonly` prefix: only the
    /// embedder writes there, in-process.
    pub fn writable(&self, key: &[u8]) -> bool {
        !self.readonly.iter().any(|p| key.starts_with(p.as_bytes()))
    }

    /// May a gRPC caller delete the range `[key, end)`? Not when it may reach a
    /// `readonly` key.
    pub fn range_writable(&self, key: &[u8], end: &[u8]) -> bool {
        !self.readonly.iter().any(|p| range_overlaps(key, end, p.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The policy ytserv configures for SKV, used as the worked example.
    fn skv() -> PrefixPolicy {
        PrefixPolicy::new(
            "/secret/",
            "/secret/encrypted/,/secret/inquiry/,/secret/@meta/",
            "/secret/",
            "/secret/",
        )
    }

    #[test]
    fn an_unconfigured_policy_changes_nothing() {
        let p = PrefixPolicy::default();
        assert!(p.is_empty());
        assert!(p.propagates(b"/secret/db_pw"));
        assert!(p.may_serve(b"/secret/db_pw", "10.0.0.7:1".parse().ok()));
    }

    #[test]
    fn the_subtree_is_local_and_the_exceptions_travel() {
        let p = skv();
        assert!(!p.propagates(b"/secret/db_pw"), "the bare namespace is local");
        assert!(!p.propagates(b"/secret/share/db_pw"), "share is a local drop point");
        assert!(p.propagates(b"/secret/encrypted/db_pw"));
        assert!(p.propagates(b"/secret/inquiry/db_pw"));
        assert!(p.propagates(b"/secret/@meta/keyver"));
    }

    #[test]
    fn everything_outside_the_subtree_is_untouched() {
        let p = skv();
        assert!(p.propagates(b"/q/ch:orders/1"));
        assert!(p.propagates(b"/cluster/peers/abc"));
        assert!(p.may_serve(b"/q/ch:orders/1", "10.0.0.7:1".parse().ok()));
        assert!(p.may_serve(b"/cluster/peers/abc", "10.0.0.7:1".parse().ok()));
    }

    #[test]
    fn a_new_branch_fails_safe() {
        // the reason `local` is the subtree and `local_except` carves out of it:
        // a branch nobody listed stays local rather than silently propagating
        let p = skv();
        assert!(!p.propagates(b"/secret/newthing/db_pw"));
        // and a typo in an exception does not leak either
        assert!(!p.propagates(b"/secret/encrypt/db_pw"));
    }

    #[test]
    fn the_subtree_is_refused_over_the_network() {
        let p = skv();
        let far: std::net::SocketAddr = "10.0.0.7:2379".parse().unwrap();
        for k in [&b"/secret/db_pw"[..], b"/secret/encrypted/db_pw", b"/secret/@meta/keyver"] {
            assert!(!p.may_serve(k, Some(far)), "{} served remotely", String::from_utf8_lossy(k));
        }
        for lo in ["127.0.0.1:2379", "[::1]:2379"] {
            assert!(p.may_serve(b"/secret/db_pw", lo.parse().ok()));
        }
        // no address: unix socket or in-process, which is us
        assert!(p.may_serve(b"/secret/db_pw", None));
    }

    #[test]
    fn the_two_policies_are_independent() {
        // a key may propagate and still be none of a remote client's business
        let p = skv();
        assert!(p.propagates(b"/secret/encrypted/db_pw"));
        assert!(!p.may_serve(b"/secret/encrypted/db_pw", "10.0.0.7:1".parse().ok()));
    }

    #[test]
    fn a_zone_scoped_key_stays_in_its_zone() {
        let p = skv();
        // propagates, but only to the same zone
        assert!(p.propagates_to_zone(b"/secret/encrypted/db_pw", "east", "east"));
        assert!(!p.propagates_to_zone(b"/secret/encrypted/db_pw", "east", "west"));
        assert!(!p.propagates_to_zone(b"/secret/@meta/keyver", "east", "west"),
            "@meta is zone-scoped too, which is what gives each zone its own key version");
        // a flat cluster is one unnamed zone: unchanged behaviour
        assert!(p.propagates_to_zone(b"/secret/encrypted/db_pw", "", ""));
        // local keys do not propagate at all, zone or no zone
        assert!(!p.propagates_to_zone(b"/secret/db_pw", "east", "east"));
        // and nothing outside the subtree is zone-scoped
        assert!(p.propagates_to_zone(b"/q/ch:orders/1", "east", "west"));
        assert!(!p.is_zone_scoped(b"/q/ch:orders/1"));
    }

    #[test]
    fn an_unzoned_policy_does_not_scope_anything() {
        let p = PrefixPolicy::new("/secret/", "/secret/encrypted/", "/secret/", "");
        assert!(!p.is_zone_scoped(b"/secret/encrypted/db_pw"));
        // with no zone prefixes, a differing zone is irrelevant
        assert!(p.propagates_to_zone(b"/secret/encrypted/db_pw", "east", "west"));
    }

    fn far() -> Option<std::net::SocketAddr> { "10.0.0.7:2379".parse().ok() }

    #[test]
    fn an_exception_to_local_only_is_served_to_peers() {
        // D15: the branches meant to travel must reach a peer's KV API, or a
        // forwarded write of one is refused (E1)
        let p = skv().with_local_only_except("/secret/encrypted/");
        assert!(p.may_serve(b"/secret/encrypted/db_pw", far()));
        assert!(!p.may_serve(b"/secret/db_pw", far()));
        assert!(!p.may_serve(b"/secret/@meta/keyver", far()));
    }

    #[test]
    fn a_remote_range_over_a_local_only_subtree_is_refused() {
        let p = skv();
        // E2: everything, from a remote caller
        assert!(!p.range_may_serve(b"", &[0], far()));
        // a range that starts before the subtree and runs into it
        assert!(!p.range_may_serve(b"/r", b"/t", far()));
        // the subtree's own prefix range, and a single key in it
        assert!(!p.range_may_serve(b"/secret/", b"/secret0", far()));
        assert!(!p.range_may_serve(b"/secret/db_pw", b"", far()));
        // ranges that never reach it
        assert!(p.range_may_serve(b"/q/", b"/q0", far()));
        assert!(p.range_may_serve(b"/a", b"/b", far()));
        assert!(p.range_may_serve(b"/t", &[0], far()), "from past the subtree on");
        // loopback and in-process are local
        assert!(p.range_may_serve(b"", &[0], "127.0.0.1:2379".parse().ok()));
        assert!(p.range_may_serve(b"", &[0], None));
    }

    #[test]
    fn a_range_wholly_inside_an_exception_is_served() {
        let p = skv().with_local_only_except("/secret/encrypted/");
        assert!(p.range_may_serve(b"/secret/encrypted/", b"/secret/encrypted0", far()));
        assert!(p.range_may_serve(b"/secret/encrypted/db_pw", b"", far()));
        assert!(!p.range_may_serve(b"/secret/", b"/secret0", far()), "the whole subtree is more than the exception");
    }

    #[test]
    fn a_readonly_prefix_refuses_every_api_write() {
        let p = PrefixPolicy::default().with_readonly("/secret/");
        assert!(!p.is_empty());
        assert!(!p.writable(b"/secret/encrypted/x"));
        assert!(p.writable(b"/q/ch:orders/1"));
        assert!(!p.range_writable(b"", &[0]), "a delete of everything reaches it");
        assert!(!p.range_writable(b"/secret/", b"/secret0"));
        assert!(p.range_writable(b"/q/", b"/q0"));
        // and nothing else changes
        assert!(p.propagates(b"/secret/x"));
        assert!(p.may_serve(b"/secret/x", far()));
    }

    #[test]
    fn prefix_ends_follow_etcd() {
        assert_eq!(prefix_end(b"/secret/"), Some(b"/secret0".to_vec()));
        assert_eq!(prefix_end(&[b'a', 0xff]), Some(b"b".to_vec()));
        assert_eq!(prefix_end(&[0xff, 0xff]), None);
        assert!(is_remote(far()));
        assert!(!is_remote(None));
        assert!(!is_remote("[::1]:1".parse().ok()));
    }

    #[test]
    fn a_non_utf8_key_is_handled_and_not_panicked_on() {
        let p = skv();
        // etcd keys are bytes; a lossy compare must not panic and must not
        // accidentally match a prefix
        assert!(p.propagates(&[0xff, 0xfe, 0x00]));
        assert!(p.may_serve(&[0xff, 0xfe, 0x00], "10.0.0.7:1".parse().ok()));
    }
}
