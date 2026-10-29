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

    /// [q-dead] Seconds a queue consumer's node may be down - not reachable as a
    /// peer by the dispatching node - before its consumers count as gone: their
    /// registrations withdrawn on their behalf, and what they were handed sent to
    /// the next consumer (at least once, for those). Unset: 300. 0: never.
    #[arg(long, required = false)]
    pub queue_down_timeout: Option<u64>,

    /// [bridge] The etcd queue the binary's bridge consumes and sends to `bridge_to`,
    /// acknowledging each message once the broker took it. Feature `bridge`; one of
    /// `bridge_queue`, `bridge_prefix`, `bridge_listen`. Empty: no bridge.
    #[arg(long, required = false, default_value = "")]
    pub bridge_queue: String,

    /// [bridge] Instead of a queue, a key prefix (`/config/`) whose puts and deletes
    /// the bridge sends; nothing acknowledged, a change while the watch is down missed.
    #[arg(long, required = false, default_value = "")]
    pub bridge_prefix: String,

    /// [bridge] Be the AMQP peer on `host:port`: a JMS client's consumer of queue X
    /// is an etcd consumer of `/q/X`. No `bridge_to` then.
    #[arg(long, required = false, default_value = "")]
    pub bridge_listen: String,

    /// [bridge] The user a client of `bridge_listen` logs in as (SASL PLAIN);
    /// anonymous when empty.
    #[arg(long, required = false, default_value = "")]
    pub bridge_listen_user: String,

    /// [bridge] That user's password.
    #[arg(long, required = false, default_value = "")]
    pub bridge_listen_password: String,

    /// [bridge] The broker: amqp://, amqps://, mqtt://, mqtts://, kafka:// or
    /// kafkas://, then `[user:password@]host:port` (Kafka: `host:port,host:port`).
    #[arg(long, required = false, default_value = "")]
    pub bridge_to: String,

    /// [bridge] The AMQP address (JMS queue), MQTT topic prefix or Kafka topic.
    /// Empty: the queue's name, or the prefix's (`/a/b/` is `a.b`).
    #[arg(long, required = false, default_value = "")]
    pub bridge_address: String,

    /// [bridge] This consumer's UUID in the queue registry; keep it across restarts
    /// to keep one registration. Empty: a new one at each start.
    #[arg(long, required = false, default_value = "")]
    pub bridge_consumer: String,

    /// [bridge] The JMS message type: auto (text when the value is UTF-8), text or bytes.
    #[arg(long, required = false, default_value = "auto")]
    pub bridge_body: String,

    /// [bridge] AMQP: JMSXGroupID is the message key, each key pinned to one consumer.
    #[arg(long, required = false, default_value = "false")]
    pub bridge_group_by_key: bool,

    /// [bridge] Add the CloudEvents attributes, in the target protocol's binding.
    #[arg(long, required = false, default_value = "false")]
    pub bridge_cloudevents: bool,

    /// [bridge] The CloudEvents `type`. Empty: etcd.queue.message.
    #[arg(long, required = false, default_value = "")]
    pub bridge_ce_type: String,

    /// [bridge] An etcd queue for messages no broker will take. Empty: they are retried.
    #[arg(long, required = false, default_value = "")]
    pub bridge_dead_letter: String,

    /// [bridge] Deliveries held between etcd and the broker (0: 64).
    #[arg(long, required = false, default_value = "64")]
    pub bridge_buffer: u32,

    /// [bridge] Seconds to wait for the broker: to connect, and to take one message (0: 30).
    #[arg(long, required = false, default_value = "30")]
    pub bridge_send_timeout: u64,

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

    /// Phase of v2store deprecation: 'not_yet', 'write_only',
    /// 'write_only_drop_data' or 'gone'.
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
    /// Setting it also enables client cert authentication, whatever `client_cert_auth` says.
    #[arg(long, required = false, default_value = "")]
    pub trusted_ca_file : String, // ''

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

    /// Enable peer client cert authentication.
    /// It's recommended to enable peer client cert authentication to prevent attacks from unauthenticated forged peers (e.g. CVE_2023_44487).
    #[arg(long, required = false, default_value = "false")]
    pub peer_client_cert_auth : bool, // ''

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

/// [i18n] The command line. Its texts - about, after_help, each argument's help -
/// are not these doc comments but the help.<id> messages (locales/<language>.yml), in the language of
/// `crate::i18n::lang()`: see [EtcdCliArgs::command_in].
#[derive(Parser)]
#[command(after_help = "https://etcd.io/docs/v3.5/op-guide/configuration/")]
pub struct EtcdCliArgs {
    /// Input files (not used)
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
        let m = Self::command_in(&crate::i18n::lang()).get_matches_from(args);
        let mut a = <Self as clap::FromArgMatches>::from_arg_matches(&m).unwrap_or_else(|e| e.exit());
        a.given = m.ids()
            .filter(|id| m.value_source(id.as_str()) == Some(clap::parser::ValueSource::CommandLine))
            .map(|id| id.as_str().to_string())
            .collect();
        a
    }

    /// [i18n] The command line, its texts in `lang` (`en`, `ru`, ...; any other is
    /// English): the about, the after_help, the headings, and each argument's help
    /// from `help.<arg id>` - the id is the field name - in locales/<language>.yml. The
    /// doc comments stay the source's documentation; `--help` shows these.
    pub fn command_in(lang: &str) -> clap::Command {
        let t = |key: &str| rust_i18n::t!(key, locale = lang).into_owned();
        let header = clap::builder::styling::Styles::default().get_usage().clone();
        let options = t("help.options");
        let arguments = t("help.arguments");
        <Self as clap::CommandFactory>::command()
            // clap's own -h/--help, made here to carry a translated help
            .disable_help_flag(true)
            // help under its option: the longest option names are some 90 columns
            .next_line_help(true)
            .arg(clap::Arg::new("help").short('h').long("help").action(clap::ArgAction::Help))
            .about(t("help.about"))
            .long_about(None)
            .after_help(t("help.after_help"))
            .help_template(format!("{{before-help}}{{about-with-newline}}\n{}{}{} {{usage}}\n\n{{all-args}}{{after-help}}",
                header.render(), t("help.usage"), header.render_reset()))
            .mut_args(|a| {
                let help = t(&format!("help.{}", a.get_id()));
                let heading = if a.is_positional() { arguments.clone() } else { options.clone() };
                let a = a.long_help(None).help_heading(heading);
                if !a.get_action().takes_values() {
                    return a.help(help);
                }
                // clap's "[default: ...]" is English: said here, and not at all when empty
                let default = a.get_default_values().iter().map(|v| v.to_string_lossy()).collect::<Vec<_>>().join(",");
                if default.is_empty() {
                    a.help(help).hide_default_value(true)
                } else {
                    a.help(format!("{} {}", help, rust_i18n::t!("help.default", locale = lang, value = default))).hide_default_value(true)
                }
            })
    }

    /// The config file's settings, under the ones the command line named; the
    /// rest as clap defaults them. (Before, every default overrode the file too:
    /// the merged twin held a value for each field that had a `default_value`.)
    pub fn parse_from<R: Read>(&mut self, input: BufReader<R>) -> Result<EtcdConfig, String> {
        let err = |e: &dyn std::fmt::Display| tr!("cli.config_file", file = self.config_path.display(), err = e);
        // serde-saphyr (no unsafe code, unlike serde_yaml's libyaml port) has no YAML
        // value type of its own: the file and the settings meet as JSON values
        let file: serde_json::Value = serde_saphyr::from_reader(input).map_err(|e| err(&e))?;
        let mut merged = serde_json::to_value(&self.config).map_err(|e| err(&e))?;
        if let (serde_json::Value::Object(m), serde_json::Value::Object(f)) = (&mut merged, file) {
            for (k, v) in f {
                if !self.given.contains(&k) {
                    m.insert(k, v);
                }
            }
        }
        serde_json::from_value(merged).map_err(|e| err(&e))
    }
}

impl EtcdConfig {
    /// [q-dead] `queue_down_timeout` as a duration: 300 s unset, `None` for 0
    /// (never).
    pub fn down_timeout(&self) -> Option<std::time::Duration> {
        match self.queue_down_timeout {
            None => Some(std::time::Duration::from_secs(300)),
            Some(0) => None,
            Some(s) => Some(std::time::Duration::from_secs(s)),
        }
    }

    /// [bridge] a bridge asked for: a queue, a prefix, a listener or a broker named
    pub fn bridge_given(&self) -> bool {
        [&self.bridge_queue, &self.bridge_prefix, &self.bridge_listen, &self.bridge_to].iter().any(|s| !s.trim().is_empty())
    }

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
                clients.remove(format!("http://{}",h).as_str());
            }
        }
        clients
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// this node is left out of its own peers, its listen url written with the scheme
    /// or without it
    #[test]
    fn a_node_is_not_its_own_peer() {
        let mut c = EtcdConfig::with_defaults();
        c.initial_advertise_peer_urls = "http://localhost:2379,http://other:2379".to_string();
        for listen in ["localhost:2379", "http://localhost:2379"] {
            c.listen_client_urls = listen.to_string();
            assert_eq!(c.peers(), HashSet::from(["http://other:2379"]), "{}", listen);
        }
    }

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

    /// An empty file, or one of comments only, sets nothing: the command line and
    /// the defaults stand (serde_yaml read it as null, and serde-saphyr does too)
    #[test]
    fn an_empty_or_comment_only_file_changes_nothing() {
        for file in ["", "# nothing set here\n", "---\n"] {
            let mut a = EtcdCliArgs::parse_args_from(["etcd", "--name", "cli-name"]);
            let c = a.parse_from(BufReader::new(file.as_bytes())).unwrap_or_else(|e| panic!("{:?}: {}", file, e));
            assert_eq!((c.name.as_str(), c.term, c.listen_client_urls.as_str()), ("cli-name", 1, "localhost:2379"), "{:?}", file);
        }
    }

    /// A file that does not parse, or that sets a value of the wrong type, is
    /// refused, and the message names the file
    #[test]
    fn a_bad_file_is_refused_naming_the_file() {
        let mut a = EtcdCliArgs::parse_args_from(["etcd", "--config", "/etc/etcd/etcd.yml"]);
        for file in ["name: [unclosed\n", "term: seven\n"] {
            let e = a.parse_from(BufReader::new(file.as_bytes())).err().unwrap_or_else(|| panic!("{:?} accepted", file));
            assert!(e.starts_with("ETCD-E001 ") && e.contains("[/etc/etcd/etcd.yml]"), "{:?}: {}", file, e);
        }
    }

    /// the help. keys (locales/en.yml - every language has the same, i18n.rs tests it),
    /// without `help.`
    fn help_keys() -> std::collections::BTreeSet<String> {
        let f = format!("{}/locales/en.yml", env!("CARGO_MANIFEST_DIR"));
        let y: serde_json::Value = serde_saphyr::from_str(&std::fs::read_to_string(f).unwrap()).unwrap();
        y.as_object().unwrap().keys().filter_map(|k| k.strip_prefix("help.")).map(str::to_string).collect()
    }

    /// the help. keys that are the command's own texts, not an argument's
    const COMMAND_TEXTS: [&str; 6] = ["about", "after_help", "usage", "options", "arguments", "default"];

    /// [i18n] every argument has its help.<id>, and every help. key is an argument's or
    /// the command's - so i18n.rs, which takes help. keys as used, takes no stale one
    #[test]
    fn every_argument_has_its_help_and_every_help_an_argument() {
        let ids: std::collections::BTreeSet<String> = EtcdCliArgs::command_in("en").get_arguments()
            .map(|a| a.get_id().to_string()).collect();
        assert!(ids.len() > 90 && ids.contains("help") && ids.contains("config_path"), "{}", ids.len());
        let keys = help_keys();
        let missing: Vec<&String> = ids.iter().filter(|i| !keys.contains(*i)).collect();
        assert!(missing.is_empty(), "no help.<id> for {:?}", missing);
        let stale: Vec<&String> = keys.iter().filter(|k| !ids.contains(*k) && !COMMAND_TEXTS.contains(&k.as_str())).collect();
        assert!(stale.is_empty(), "help. keys naming no argument: {:?}", stale);
    }

    /// [i18n] every language's command line is the English one translated: the same
    /// arguments - names, defaults, actions - each help and the about translated
    /// (Russian, Chinese and Japanese told by their script, French and German by
    /// differing from English); the unreviewed ones say so, and how to get English
    #[test]
    fn every_command_line_is_the_english_one_translated() {
        let args = |c: &clap::Command| c.get_arguments()
            .map(|a| (a.get_id().to_string(), a.get_long().map(str::to_string), a.get_short(),
                a.get_default_values().to_vec(), format!("{:?}", a.get_action())))
            .collect::<Vec<_>>();
        let texts = |c: &clap::Command| {
            let mut t = vec![("about".to_string(), c.get_about().unwrap().to_string()),
                ("after_help".to_string(), c.get_after_help().unwrap().to_string())];
            t.extend(c.get_arguments().map(|a| (a.get_id().to_string(), a.get_help().unwrap().to_string())));
            t
        };
        let en = EtcdCliArgs::command_in("en");
        for l in ["ru", "fr", "de", "zh", "ja"] {
            let c = EtcdCliArgs::command_in(l);
            assert_eq!(args(&en), args(&c), "{}", l);
            for ((what, e), (_, t)) in texts(&en).iter().zip(texts(&c).iter()) {
                let has = |from: char, to: char| t.chars().any(|c| (from..=to).contains(&c));
                let translated = match l {
                    "ru" => has('\u{0400}', '\u{04FF}'),
                    "zh" => has('\u{4E00}', '\u{9FFF}'),
                    "ja" => has('\u{3040}', '\u{30FF}'),
                    _ => t != e,
                };
                assert!(translated, "{} {} is not translated: {}", l, what, t);
            }
            let about = c.get_about().unwrap().to_string();
            assert_eq!(l == "ru", !about.contains("LANG=en"), "{}: the notice that it is not reviewed: {}", l, about);
        }
        // a language without messages is English
        assert_eq!(texts(&EtcdCliArgs::command_in("xx")), texts(&en));
    }

    /// [i18n] --help in Russian: Russian text, and every option's name as it is
    #[test]
    fn the_help_in_russian_keeps_the_option_names() {
        let mut c = EtcdCliArgs::command_in("ru");
        let help = c.render_help().to_string();
        for text in ["Использование:", "Параметры", "Префиксы ключей, которые никогда не передаются", "[по умолчанию: localhost:2379]",
            "https://etcd.io/docs/v3.5/op-guide/configuration/"] {
            assert!(help.contains(text), "no {:?} in\n{}", text, help);
        }
        for a in c.get_arguments().filter_map(|a| a.get_long()) {
            assert!(help.contains(&format!("--{}", a)), "no --{} in\n{}", a, help);
        }
        assert!(!help.contains("Options") && !help.contains("[default:"), "English left in\n{}", help);
    }

    /// the command line parses as before the translation: the help is all it changed
    #[test]
    fn the_translated_command_line_parses_the_same() {
        for l in ["en", "ru", "ja"] {
            let m = EtcdCliArgs::command_in(l).try_get_matches_from(["etcd", "--name", "n1", "--pre-vote", "--config", "x.yml"]).unwrap();
            let a = <EtcdCliArgs as clap::FromArgMatches>::from_arg_matches(&m).unwrap();
            assert_eq!((a.config.name.as_str(), a.config.pre_vote, a.config_path.to_str()), ("n1", true, Some("x.yml")), "{}", l);
            let e = EtcdCliArgs::command_in(l).try_get_matches_from(["etcd", "--help"]).unwrap_err();
            assert_eq!(e.kind(), clap::error::ErrorKind::DisplayHelp, "{}", l);
        }
    }
}
