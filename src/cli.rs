use clap_serde_derive::{
    clap::{self},
    serde::Serialize,
    ClapSerde,
};

#[derive(ClapSerde, Clone)]
pub struct Config {
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

#[derive(ClapSerde, Serialize)]
#[derive(Debug)]
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

