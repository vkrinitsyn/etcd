use std::collections::HashSet;
use std::io::{BufReader, Read};
use clap::Parser;
use serde::{Deserialize, Serialize};

/// The node's settings: command-line arguments (clap) and a config file (serde).
///
/// [no-agpl] Plain `clap::Args` + serde. `clap-serde-derive` made this before - an
/// `Opt` twin of every field and a merge - and it is AGPL-3.0, which the binaries
/// linking this crate cannot ship under. `Default` stays what it was: each type's
/// default (empty, 0, false); the `default_value`s are clap's, see `with_defaults`.
#[derive(clap::Args, Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EtcdConfig {
    /// Node UUID
    #[arg(long, required = false, default_value = "")]
    pub node : String,

    /// Cluster UUID
    #[arg(long, required = false, default_value = "")]
    pub cluster: String,

    /// Logical term (epoch) for this node, incremented on restart/reconfigure.
    #[arg(long, required = false, default_value = "1")]
    pub term: u64,

    /// Key prefixes that never propagate to peers, comma separated.
    ///
    /// A key under one of these lives on the node that wrote it and nowhere
    /// else. Empty (the default) means every key propagates, which is
    /// byte-for-byte the behaviour before this existed.
    #[arg(long, required = false, default_value = "")]
    pub local_prefix: String,

    /// Sub-prefixes of `local_prefix` that DO propagate, comma separated.
    ///
    /// Carved out of the local subtree rather than listed as an allow-list, so
    /// a branch nobody named stays local: the default for anything new is "does
    /// not leave the node", which is the safe direction for a subtree that
    /// exists because its contents are confidential.
    #[arg(long, required = false, default_value = "")]
    pub local_prefix_except: String,

    /// Key prefixes served only to a loopback caller, comma separated.
    ///
    /// Independent of `local_prefix`: a value may legitimately propagate
    /// between nodes while still being none of a remote client's business.
    #[arg(long, required = false, default_value = "")]
    pub local_only_prefix: String,

    /// Branches of `local_only_prefix` that ARE served to remote callers, comma
    /// separated - the branches meant to travel, whose writes a peer forwards
    /// through the KV API (without this a forwarded write is refused). Empty: none.
    #[arg(long, required = false, default_value = "")]
    pub local_only_prefix_except: String,

    /// Key prefixes no API caller may write, comma separated: put, delete and txn
    /// are refused there, and only the embedder writes them, in-process (a mirror
    /// it keeps from its own source of truth). Empty: none.
    #[arg(long, required = false, default_value = "")]
    pub readonly_prefix: String,

    /// Key prefixes that propagate only to peers in this node's zone.
    ///
    /// Composes with `local_prefix`: a key may propagate and still be confined
    /// to one zone. Empty means no key is zone-scoped, which is today's
    /// behaviour.
    #[arg(long, required = false, default_value = "")]
    pub zone_prefix: String,

    /// This node's zone. Empty means unzoned, and two unzoned nodes share the
    /// same (unnamed) zone.
    #[arg(long, required = false, default_value = "")]
    pub zone: String,

    /// Zone of each peer: `host:port=zone`, comma separated.
    ///
    /// Supplied by the embedder, which already knows the topology. The etcd
    /// member protocol carries no zone and extending it would break API
    /// compatibility, so labels come from configuration rather than from a
    /// handshake.
    #[arg(long, required = false, default_value = "")]
    pub peer_zones: String,

// all fields bellow copied from etcd configuration

    /// Human-readable name for this member.
    #[arg(long, required = false, default_value = "default")]
    pub name: String,
    /// Path to the data directory.
    /// NOT USE in this impl.
    #[arg(long, required = false, default_value = "${name}.etcd")]
    pub data_dir: String,

    /// Path to the dedicated wal directory.
    /// NOT USE in this impl.
    #[arg(long, required = false, default_value = "")]
    pub wal_dir : String, // ''

    /// Number of committed transactions to trigger a snapshot to disk.
    /// NOT USE in this impl.
    #[arg(long, required = false, default_value = "100000")]
    pub snapshot_count: u32,

    /// Time (in milliseconds) of a heartbeat interval.
    /// NOT USE in this impl.
    #[arg(long, required = false, default_value = "100")]
    pub heartbeat_interval: u32,

    /// Time (in milliseconds) for an election to timeout. See tuning documentation for details.
    /// NOT USE in this impl.
    #[arg(long, required = false, default_value = "1000")]
    pub election_timeout: u32,

    /// Whether to fast_forward initial election ticks on boot for faster election.
    /// NOT USE in this impl.
    #[arg(long, required = false, default_value = "true")]
    pub initial_election_tick_advance: bool,

    /// List of URLs to listen on for peer traffic.
    /// The URLs needed to be a comma-separated list.
    #[arg(long, required = false, default_value = "http://localhost:2380")]
    pub listen_peer_urls: String,

    /// List of URLs to listen on for client grpc traffic and http as long as --listen-client-http-urls is not specified.
    /// The URLs needed to be a comma-separated list.
    #[arg(long, required = false, default_value = "localhost:2379")]
    pub listen_client_urls: String,

    ///List of URLs to listen on for http only client traffic. Enabling this flag removes http services from --listen-client-urls.
    #[arg(long, required = false, default_value = "")]
    pub listen_client_http_urls : String,

    /// ClickHouse HTTP endpoint for /q/ch:<table>/ queues, e.g. http://127.0.0.1:8123
    /// (may carry userinfo). Empty disables the built-in consumer, and such a
    /// queue then behaves like any other rather than dropping messages.
    #[arg(long, required = false, default_value = "")]
    pub clickhouse_url: String,

    /// Database for /q/ch:<table>/ queues whose name does not carry one.
    #[arg(long, required = false, default_value = "")]
    pub clickhouse_db: String,

    /// Maximum number of snapshot files to retain (0 is unlimited).
    #[arg(long, required = false, default_value = "5")]
    pub max_snapshots: u32,

    /// Maximum number of wal files to retain (0 is unlimited).
    #[arg(long, required = false, default_value = "5")]
    pub max_wals: u32,

    /// Raise alarms when backend size exceeds the given quota (0 defaults to low space quota).
    #[arg(long, required = false, default_value = "0")]
    pub quota_backend_bytes: u32,

    /// BackendFreelistType specifies the type of freelist that boltdb backend uses(array and map are supported types).
    #[arg(long, required = false, default_value = "map")]
    pub backend_bbolt_freelist_type: String,

    /// BackendBatchInterval is the maximum time before commit the backend transaction.
    #[arg(long, required = false, default_value = "")]
    pub backend_batch_interval : String, // ''

    /// BackendBatchLimit is the maximum operations before commit the backend transaction.
    #[arg(long, required = false, default_value = "0")]
    pub backend_batch_limit: u32,

    /// Maximum number of operations permitted in a transaction.
    #[arg(long, required = false, default_value = "128")]
    pub max_txn_ops: u32,

    /// Maximum client request size in bytes the server will accept.
    #[arg(long, required = false, default_value = "1572864")]
    pub max_request_bytes: u32,

    /// Minimum duration interval that a client should wait before pinging server.
    #[arg(long, required = false, default_value = "5s")]
    pub grpc_keepalive_min_time: String,

    /// Frequency duration of server_to_client ping to check if a connection is alive (0 to disable).
    #[arg(long, required = false, default_value = "2h")]
    pub grpc_keepalive_interval: String,

    /// Additional duration of wait before closing a non_responsive connection (0 to disable).
    #[arg(long, required = false, default_value = "20s")]
    pub grpc_keepalive_timeout: String,

    /// Enable to set socket option SO_REUSEPORT on listeners allowing rebinding of a port already in use.
    #[arg(long, required = false, default_value = "false")]
    pub socket_reuse_port : bool, // ''

    /// Enable to set socket option SO_REUSEADDR on listeners allowing binding to an address in TIME_WAIT state.
    #[arg(long, required = false, default_value = "false")]
    pub socket_reuse_address : bool, // ''




    /// List of this member's peer URLs to advertise to the rest of the cluster.
    /// The URLs needed to be a comma-separated list: http://localhost:2380
    #[arg(long, required = false, default_value = "")]
    pub initial_advertise_peer_urls: String,

    /// Comma separated string of initial cluster configuration for bootstrapping.
    /// Example: initial-cluster: "infra0=http://10.0.1.10:2380,infra1=http://10.0.1.11:2380,infra2=http://10.0.1.12:2380"
    #[arg(long, required = false, default_value = "http://localhost:2380")]
    pub initial_cluster: String, // 'default='

    /// Initial cluster state ('new' or 'existing').
    #[arg(long, required = false, default_value = "new")]
    pub initial_cluster_state: String,

    /// Initial cluster token for the etcd cluster during bootstrap.

    /// Specifying this can protect you from unintended cross_cluster interaction when running multiple clusters.
    #[arg(long, required = false, default_value = "etcd-cluster")]
    pub initial_cluster_token: String,


    /// List of this member's client URLs to advertise to the public.

    /// The client URLs advertised should be accessible to machines that talk to etcd cluster. etcd client libraries parse these URLs to connect to the cluster.
    /// The URLs needed to be a comma-separated list.
    #[arg(long, required = false, default_value = "http://localhost:2379")]
    pub advertise_client_urls: String,

    /// Discovery URL used to bootstrap the cluster.
    #[arg(long, required = false, default_value = "")]
    pub discovery : String, // ''


    /// Expected behavior ('exit' or 'proxy') when discovery services fails.

    /// "proxy" supports v2 API only.
    #[arg(long, required = false, default_value = "proxy")]
    pub discovery_fallback: String,


    /// HTTP proxy to use for traffic to discovery service.
    #[arg(long, required = false, default_value = "")]
    pub discovery_proxy : String, // ''

    /// DNS srv domain used to bootstrap the cluster.
    #[arg(long, required = false, default_value = "")]
    pub discovery_srv : String, // ''

    /// Suffix to the dns srv name queried when bootstrapping.
    #[arg(long, required = false, default_value = "")]
    pub discovery_srv_name : String, // ''

    /// Reject reconfiguration requests that would cause quorum loss.
    #[arg(long, required = false, default_value = "true")]
    pub strict_reconfig_check: bool,

    /// Enable the raft Pre_Vote algorithm to prevent disruption when a node that has been partitioned away rejoins the cluster.
    #[arg(long, required = false, default_value = "true")]
    pub pre_vote: bool,

    /// Auto compaction retention length. 0 means disable auto compaction.
    #[arg(long, required = false, default_value = "0")]
    pub auto_compaction_retention: u32,

    /// Interpret 'auto_compaction_retention' one of: periodic|revision. 'periodic' for duration based retention, defaulting to hours if no time unit is provided (e.g. '5m'). 'revision' for revision number based retention.
    #[arg(long, required = false, default_value = "periodic")]
    pub auto_compaction_mode: String,

    /// Accept etcd V2 client requests. Deprecated and to be decommissioned in v3.6.
    #[arg(long, required = false, default_value = "false")]
    pub enable_v2 : bool, // ''

    #[arg(long, required = false, default_value = "not_yet")]
    pub v2_deprecation: String,

    // Phase of v2store deprecation. Allows to opt_in for higher compatibility mode.

    // Supported values:

    // 'not_yet'
    //
    //
    // // Issues a warning if v2store have meaningful content (default in v3.5)

    // 'write_only'
    //
    //  // Custom v2 state is not allowed (planned default in v3.6)

    // 'write_only_drop_data' // Custom v2 state will get DELETED !

    // 'gone'
    //
    //
    //  // v2store is not maintained any longer. (planned default in v3.7)


    // client_transport_security:


    /// Path to the client server TLS cert file.
    #[arg(long, required = false, default_value = "")]
    pub cert_file : String, // ''

    /// Path to the client server TLS key file.
    #[arg(long, required = false, default_value = "")]
    pub key_file : String, // ''

    /// Enable client cert authentication.

    /// It's recommended to enable client cert authentication to prevent attacks from unauthenticated clients (e.g. CVE_2023_44487), especially when running etcd as a public service.
    #[arg(long, required = false, default_value = "false")]
    pub client_cert_auth : bool, // ''
    /// Path to the client certificate revocation list file.
    #[arg(long, required = false, default_value = "")]
    pub client_crl_file : String, // ''

    /// Allowed TLS hostname for client cert authentication.
    #[arg(long, required = false, default_value = "")]
    pub client_cert_allowed_hostname : String, // ''

    /// Path to the client server TLS trusted CA cert file.

    /// //Note setting this parameter will also automatically enable client cert authentication no matter what value is set for `
    ///
    #[arg(long, required = false, default_value = "")]
    pub trusted_ca_file : String, // ''#[arg(long, required = false, default_value = "")]
    // client_cert_auth`.

    /// Client TLS using generated certificates.
    #[arg(long, required = false, default_value = "false")]
    pub auto_tls : bool, // ''

    // peer_transport_security:


    /// Path to the peer server TLS cert file.
    #[arg(long, required = false, default_value = "")]
    pub peer_cert_file : String, // ''

    /// Path to the peer server TLS key file.
    #[arg(long, required = false, default_value = "")]
    pub peer_key_file : String, // ''

    #[arg(long, required = false, default_value = "false")]
    pub peer_client_cert_auth : bool, // ''

    /// Enable peer client cert authentication.

    /// It's recommended to enable peer client cert authentication to prevent attacks from unauthenticated forged peers (e.g. CVE_2023_44487).

    /// Path to the peer server TLS trusted CA file.
    #[arg(long, required = false, default_value = "")]
    pub peer_trusted_ca_file : String, // ''

    /// Required CN for client certs connecting to the peer endpoint.
    #[arg(long, required = false, default_value = "")]
    pub peer_cert_allowed_cn : String, // ''

    /// Allowed TLS hostname for inter peer authentication.
    #[arg(long, required = false, default_value = "")]
    pub peer_cert_allowed_hostname : String, // ''

    ///   Peer TLS using self-generated certificates if --peer-key-file and --peer-cert-file are not provided.
    #[arg(long, required = false, default_value = "false")]
    pub peer_auto_tls : bool,

    /// The validity period of the client and peer certificates that are automatically generated by etcd when you specify ClientAutoTLS and PeerAutoTLS, the unit is year, and the default is 1.
    #[arg(long, required = false, default_value = "1")]
    pub self_signed_cert_validity: u32,

    /// Path to the peer certificate revocation list file.
    #[arg(long, required = false, default_value = "")]
    pub peer_crl_file : String, // ''

    /// Comma_separated list of supported TLS cipher suites between client/server and peers (empty will be auto_populated by Go).
    #[arg(long, required = false, default_value = "")]
    pub cipher_suites : String, // ''


    /// Comma_separated whitelist of origins for CORS, or cross_origin resource sharing, (empty or * means allow all).
    #[arg(long, required = false, default_value = "*")]
    pub cors: String,

    /// Acceptable hostnames from HTTP client requests, if server is not secure (empty or * means allow all).
    #[arg(long, required = false, default_value = "*")]
    pub host_whitelist: String,

    /// Minimum TLS version supported by etcd.
    #[arg(long, required = false, default_value = "TLS1.2")]
    pub tls_min_version: String,

    /// Maximum TLS version supported by etcd (empty will be auto_populated by Go).
    #[arg(long, required = false, default_value = "")]
    pub tls_max_version : String, // ''



    /// Specify a v3 authentication token type and token specific options, especially for JWT. Its format is "type,var1=val1,var2=val2,...". Possible type is 'simple' or 'jwt'. Possible variables are 'sign_method' for specifying a sign method of jwt (its possible values are 'ES256', 'ES384', 'ES512', 'HS256', 'HS384', 'HS512', 'RS256', 'RS384', 'RS512', 'PS256', 'PS384', or 'PS512'), 'pub_key' for specifying a path to a public key for verifying jwt, 'priv_key' for specifying a path to a private key for signing jwt, and 'ttl' for specifying TTL of jwt tokens.
    #[arg(long, required = false, default_value = "simple")]
    pub auth_token: String,

    /// Specify the cost / strength of the bcrypt algorithm for hashing auth passwords. Valid values are between 4 and 31.
    #[arg(long, required = false, default_value = "10")]
    pub bcrypt_cost: u32,

    /// Time (in seconds) of the auth_token_ttl.
    #[arg(long, required = false, default_value = "300")]
    pub auth_token_ttl: u32,



    /// Enable runtime profiling data via HTTP server. Address is at client URL + "/debug/pprof/"
    #[arg(long, required = false, default_value = "false")]
    pub enable_pprof : bool, // ''

    /// Set level of detail for exported metrics, specify 'extensive' to include server side grpc histogram metrics.
    #[arg(long, required = false, default_value = "basic")]
    pub metrics: String,

    /// List of URLs to listen on for the metrics and health endpoints.
    #[arg(long, required = false, default_value = "")]
    pub listen_metrics_urls : String, // ''



    /// Currently only supports 'zap' for structured logging.
    /// NOT USE in this impl.
    #[arg(long, required = false, default_value = "zap")]
    pub logger: String,

    /// Specify 'stdout' or 'stderr' to skip journald logging even when running under systemd, or list of comma separated output targets.
    #[arg(long, required = false, default_value = "default")]
    pub log_outputs: String,

    /// Configures log level. Only supports debug, info, warn, error, panic, or fatal.
    #[arg(long, required = false, default_value = "info")]
    pub log_level: String,

    /// Enable log rotation of a single log_outputs file target.
    #[arg(long, required = false, default_value = "false")]
    pub enable_log_rotation : bool,

    /// Configures log rotation if enabled with a JSON logger config. MaxSize(MB), MaxAge(days,0=no limit), MaxBackups(0=no limit), LocalTime(use computers local time), Compress(gzip)".
    #[arg(long, required = false, default_value = "{\"maxsize\": 100, \"maxage\": 0, \"maxbackups\": 0, \"localtime\": false, \"compress\": false}")]
    pub log_rotation_config_json: String,



    /// Enable experimental distributed tracing.
    #[arg(long, required = false, default_value = "false")]
    pub experimental_enable_distributed_tracing : bool,

    /// Distributed tracing collector address.
    #[arg(long, required = false, default_value = "localhost:4317")]
    pub experimental_distributed_tracing_address: String,


    /// Distributed tracing service name, must be same across all etcd instances.
    #[arg(long, required = false, default_value = "etcd")]
    pub experimental_distributed_tracing_service_name: String,


    /// Distributed tracing instance ID, must be unique per each etcd instance.
    #[arg(long, required = false, default_value = "")]
    pub experimental_distributed_tracing_instance_id : String,


    /// Number of samples to collect per million spans for OpenTelemetry Tracing (if enabled with experimental_enable_distributed_tracing flag).
    #[arg(long, required = false, default_value = "0")]
    pub experimental_distributed_tracing_sampling_rate: u32,


    // #[clap_serde]
    // #[command(flatten)]
    // pub client_transport_security: ClientConfig,
}

#[derive(clap::Args, Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct ClientConfig {
    /// Path to the client server TLS cert file.
    #[arg(long)]
    cert_file: String,
    /// Path to the client server TLS key file.
    #[arg(long, default_value = "false")]
    key_file: bool,

    /// Enable client cert authentication.
    #[arg(long, default_value = "false")]
    client_cert_auth: bool,

    /// Path to the client server TLS trusted CA cert file.
    #[arg(long)]
    trusted_ca_file: String,

    /// Client TLS using generated certificates
    #[arg(long, default_value = "false")]
    auto_tls: bool

}

#[derive(Parser)]
// author, version, about,
#[command(after_help = "https://etcd.io/docs/v3.5/op-guide/configuration/")]
pub struct EtcdCliArgs {
    /// Input files
    input: Vec<std::path::PathBuf>,

    /// Config file
    #[arg(long = "config", default_value = "config.yml")]
    pub config_path: std::path::PathBuf,

    /// Rest of arguments
    #[command(flatten)]
    pub config: EtcdConfig,

    /// The settings named on the command line (arg ids, which are the field
    /// names): they win over the config file. Filled by `parse_args`.
    #[arg(skip)]
    pub given: HashSet<String>,
}

impl EtcdCliArgs {
    /// The command line, and which settings it named rather than defaulted.
    pub fn parse_args() -> Self {
        Self::parse_args_from(std::env::args_os())
    }

    /// `parse_args` over the given arguments, the program name first.
    pub fn parse_args_from<I, T>(args: I) -> Self
    where I: IntoIterator<Item = T>, T: Into<std::ffi::OsString> + Clone {
        let m = <Self as clap::CommandFactory>::command().get_matches_from(args);
        let mut a = <Self as clap::FromArgMatches>::from_arg_matches(&m).unwrap_or_else(|e| e.exit());
        a.given = m.ids()
            .filter(|id| m.value_source(id.as_str()) == Some(clap::parser::ValueSource::CommandLine))
            .map(|id| id.as_str().to_string())
            .collect();
        a
    }

    /// The config file's settings, under the ones the command line named; the
    /// rest as clap defaults them. (Before, every default overrode the file too:
    /// the merged twin held a value for each field that had a `default_value`.)
    pub fn parse_from<R: Read>(&mut self, input: BufReader<R>) -> Result<EtcdConfig, String> {
        let err = |e: serde_yaml::Error| format!("Error in configuration file [{}]:\n{}", self.config_path.display(), e);
        let file: serde_yaml::Value = serde_yaml::from_reader(input).map_err(err)?;
        let mut merged = serde_yaml::to_value(&self.config).map_err(err)?;
        if let (serde_yaml::Value::Mapping(m), serde_yaml::Value::Mapping(f)) = (&mut merged, file) {
            for (k, v) in f {
                if k.as_str().map_or(false, |name| !self.given.contains(name)) {
                    m.insert(k, v);
                }
            }
        }
        serde_yaml::from_value(merged).map_err(err)
    }
}

impl EtcdConfig {
    /// Every setting at its clap `default_value`.
    pub fn with_defaults() -> Self {
        <EtcdCliArgs as Parser>::parse_from(["etcd"]).config
    }

    pub fn peers(&self) -> HashSet<&str> {
        let urls: HashSet<&str> = HashSet::from_iter(self.listen_client_urls.split(","));
        let mut clients: HashSet<&str> = HashSet::from_iter(self.initial_advertise_peer_urls.split(",")
            .filter(|c| c.trim().len() > 0 && c.starts_with("http")));
        for h in urls {
            if h.contains("//") {
                clients.remove(h);
            } else {
                clients.remove(format!("http:://{}",h).as_str());
            }
        }
        clients
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_defaults_stay_what_they_were() {
        // the type defaults: what clap-serde-derive's Default gave without #[default]
        let d = EtcdConfig::default();
        assert_eq!((d.name.as_str(), d.term, d.listen_client_urls.as_str()), ("", 0, ""));
        // clap's default_values
        let c = EtcdConfig::with_defaults();
        assert_eq!((c.name.as_str(), c.term, c.listen_client_urls.as_str()), ("default", 1, "localhost:2379"));
        assert!(c.pre_vote && !c.enable_v2);
    }

    #[test]
    fn a_named_argument_wins_over_the_file_and_the_file_over_a_default() {
        let mut a = EtcdCliArgs::parse_args_from(["etcd", "--name", "cli-name"]);
        assert!(a.given.contains("name") && !a.given.contains("term"), "{:?}", a.given);
        let file = "name: file-name\nterm: 7\nzone: z1\n";
        let c = a.parse_from(BufReader::new(file.as_bytes())).unwrap();
        assert_eq!(c.name, "cli-name", "named on the command line");
        assert_eq!((c.term, c.zone.as_str()), (7, "z1"), "from the file");
        assert_eq!(c.listen_client_urls, "localhost:2379", "neither: clap's default");
        assert!(a.parse_from(BufReader::new("term: [not a number]".as_bytes())).is_err());
    }
}
