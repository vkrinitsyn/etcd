//! The standalone `etcds` binary (agent.md, "Running it standalone"): the
//! config file and flags it reads, the exit codes it reports, and the signals
//! that stop it.
#![cfg(unix)]

use std::process::{Child, Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_etcds");
const CLUSTER: &str = "11111111-1111-5111-8111-111111111111";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A config file of its own for one run, removed when dropped.
struct ConfigFile(std::path::PathBuf);

impl ConfigFile {
    fn new(contents: &str) -> Self {
        let p = std::env::temp_dir().join(format!("etcds-test-{}.yml", uuid::Uuid::new_v4()));
        std::fs::write(&p, contents).unwrap();
        ConfigFile(p)
    }
    fn missing() -> Self {
        ConfigFile(std::env::temp_dir().join(format!("etcds-test-{}-absent.yml", uuid::Uuid::new_v4())))
    }
    fn arg(&self) -> String {
        self.0.display().to_string()
    }
}

impl Drop for ConfigFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn spawn(cfg: &ConfigFile, args: &[&str]) -> Child {
    Command::new(BIN)
        .arg("--config").arg(cfg.arg())
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the etcds binary starts")
}

/// Wait for a child that is expected to exit on its own, killing it if it does not.
fn exit_code(mut child: Child) -> (i32, String) {
    for _ in 0..200 {
        if let Some(status) = child.try_wait().unwrap() {
            let mut err = String::new();
            use std::io::Read;
            child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
            return (status.code().expect("exited, not signalled"), err);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    panic!("etcds did not exit on its own");
}

fn signal(child: &Child, sig: &str) {
    let ok = Command::new("kill").arg(format!("-{}", sig)).arg(child.id().to_string())
        .status().unwrap().success();
    assert!(ok, "kill -{} {}", sig, child.id());
}

/// Connect to a node the binary serves, waiting for it to bind.
async fn connect(port: u16) -> etcd_client::Client {
    let url = format!("http://127.0.0.1:{}", port);
    for _ in 0..100 {
        if let Ok(mut c) = etcd_client::Client::connect([url.as_str()], None).await {
            if c.status().await.is_ok() {
                return c;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("etcds on {} never answered", url);
}

/// The settings come from the file, the node serves the etcd API on the
/// listen url, and a termination signal stops it with exit code 0.
#[tokio::test]
async fn it_serves_from_its_config_file_and_stops_on_a_signal() {
    for (sig, level) in [("TERM", "warning"), ("INT", "info"), ("HUP", "WARN")] {
        let port = free_port();
        let cfg = ConfigFile::new(&format!(
            "listen_client_urls: 127.0.0.1:{}\ncluster: {}\nlog_level: {}\nterm: 4\n",
            port, CLUSTER, level));
        let child = spawn(&cfg, &[]);
        let mut client = connect(port).await;
        assert_eq!(client.status().await.unwrap().raft_term(), 4, "term from the file");
        client.put("k", "v", None).await.unwrap();
        assert_eq!(client.get("k", None).await.unwrap().kvs()[0].value(), b"v");

        signal(&child, sig);
        let (code, _) = exit_code(child);
        assert_eq!(code, 0, "SIG{} is an orderly stop", sig);
    }
}

/// A flag given on the command line wins over the file.
#[tokio::test]
async fn a_flag_wins_over_the_file() {
    let port = free_port();
    let cfg = ConfigFile::new(&format!(
        "listen_client_urls: 127.0.0.1:{}\ncluster: {}\nterm: 4\n", port, CLUSTER));
    let child = spawn(&cfg, &["--term", "9", "--log-level", "debug"]);
    let mut client = connect(port).await;
    assert_eq!(client.status().await.unwrap().raft_term(), 9);
    signal(&child, "TERM");
    assert_eq!(exit_code(child).0, 0);
}

/// A missing file is not an error: the flags and defaults stand.
#[tokio::test]
async fn a_missing_file_leaves_the_flags() {
    let port = free_port();
    let cfg = ConfigFile::missing();
    let listen = format!("127.0.0.1:{}", port);
    let child = spawn(&cfg, &["--listen-client-urls", &listen, "--cluster", CLUSTER,
        "--log-level", "trace"]);
    let mut client = connect(port).await;
    assert_eq!(client.status().await.unwrap().raft_term(), 1, "clap's default term");
    signal(&child, "TERM");
    assert_eq!(exit_code(child).0, 0);
}

/// A file that does not parse is refused at start, naming the file: exit 11.
#[test]
fn a_bad_config_file_exits_11() {
    let cfg = ConfigFile::new("term: [not a number\n");
    let (code, err) = exit_code(spawn(&cfg, &[]));
    assert_eq!(code, 11);
    assert!(err.contains(&cfg.arg()), "the message names the file: {}", err);
}

/// A node id that is not a UUID fails `EtcdNode::init`: exit 13.
#[test]
fn a_bad_node_id_exits_13() {
    let cfg = ConfigFile::missing();
    let (code, _) = exit_code(spawn(&cfg, &["--node", "not-a-uuid", "--log-level", "error"]));
    assert_eq!(code, 13);
}

/// A listen address that does not parse fails `serve`: exit 12.
#[test]
fn an_unparsable_listen_address_exits_12() {
    let cfg = ConfigFile::missing();
    let (code, _) = exit_code(spawn(&cfg, &["--listen-client-urls", "127.0.0.1", "--cluster", CLUSTER]));
    assert_eq!(code, 12);
}

/// A port somebody else holds: the server cannot bind, and the process exits 10.
#[test]
fn a_port_in_use_exits_10() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = held.local_addr().unwrap().to_string();
    let cfg = ConfigFile::missing();
    let (code, _) = exit_code(spawn(&cfg, &["--listen-client-urls", &listen, "--cluster", CLUSTER]));
    assert_eq!(code, 10);
    drop(held);
}
