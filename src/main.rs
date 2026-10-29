#![forbid(unsafe_code)]


#[macro_use]
extern crate slog;

use std::{fs::File, io::BufReader};
use std::process::ExitCode;

use slog::Logger;
use sloggers::Build;
use sloggers::terminal::{Destination, TerminalLoggerBuilder};
use sloggers::types::{Severity, SourceLocation};

/// etcd standalone entry point
#[tokio::main]
async fn main() -> ExitCode {
    // Parse whole args with clap
    let mut args = EtcdCliArgs::parse_args();
   
    // Get config file
    let config = if let Ok(f) = File::open(&args.config_path) {
        match args.parse_from(BufReader::new(f)) {
            Ok(a) => a,
            Err(e) => {
                // the error names the file, with its code (cli.config_file)
                eprintln!("{}", e);
                return ExitCode::from(11);
            }
        }
    } else {
        // If there is not config file return only config parsed from clap
        args.config.clone()
    };
    let log = logger(&config);
    // [bridge] its settings refused before the node starts
    let bridge = match bridge(&config) {
        Ok(b) => b,
        Err(e) => {
            crit!(log, "{}", e);
            return ExitCode::from(14);
        }
    };
    match EtcdNode::init(
        config, log.clone(),
        #[cfg(feature = "tracer")] None,
    ).await {
        Ok(c) => {
            if let Err(e) = c.serve().await {
                crit!(log, "{}", message("main.serve", &[("err", &e)]));
                return ExitCode::from(12);
            }
        }
        Err(e) => {
            crit!(log, "{}", message("main.init", &[("err", &e)]));
            return ExitCode::from(13);
        }
    }

    let Some(b) = bridge else {
        return stopped(log, wait_for_signal().await);
    };
    // [bridge] once the node serves, until the signal that stops it; a bridge that
    // ends on its own failed for good, and takes the node with it
    let within = stop_within(&b);
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let mut running = tokio::spawn(run_bridge(b, log.new(o!("bridge" => true)), async {
        let _ = stop_rx.await;
    }));
    tokio::select! {
        sig = wait_for_signal() => {
            let _ = stop.send(());
            let err = match tokio::time::timeout(within, running).await {
                Ok(Ok(Ok(()))) => None,
                Ok(Ok(Err(e))) => Some(e),
                Ok(Err(e)) => Some(e.to_string()),
                Err(_) => Some(format!("not stopped in {:?}", within)),
            };
            match err {
                // what it held may not be back on etcd's line: not an orderly stop
                Some(e) => exit_with_msg(log, message("main.bridge_stop", &[("err", &e)]), 16),
                None => stopped(log, sig),
            }
        }
        r = &mut running => {
            let err = match r {
                Ok(Ok(())) => "stopped".to_string(),
                Ok(Err(e)) => e,
                Err(e) => e.to_string(),
            };
            exit_with_msg(log, message("main.bridge_failed", &[("err", &err)]), 15)
        }
    }
}

fn stopped(log: Logger, sig: Result<String, String>) -> ExitCode {
    match sig {
        Ok(msg) => exit_with_msg(log, msg, 0),
        Err(msg) => exit_with_msg(log, msg, 10),
    }
}

/// [bridge] The bridge the settings ask for, or why not (exit code 14).
#[cfg(feature = "bridge")]
fn bridge(cfg: &EtcdConfig) -> Result<Option<etcds::bridge::node::Bridge>, String> {
    etcds::bridge::node::settings(cfg).map_err(|e| message("main.bridge", &[("err", &e)]))
}

#[cfg(feature = "bridge")]
use etcds::bridge::node::{run as run_bridge, stop_within};

/// [bridge] Without the feature there is no bridge to ask for.
#[cfg(not(feature = "bridge"))]
fn bridge(cfg: &EtcdConfig) -> Result<Option<std::convert::Infallible>, String> {
    if cfg.bridge_given() { Err(message("main.bridge_feature", &[])) } else { Ok(None) }
}

#[cfg(not(feature = "bridge"))]
async fn run_bridge(b: std::convert::Infallible, _: Logger, _: impl Future<Output = ()>) -> Result<(), String> {
    match b {}
}

#[cfg(not(feature = "bridge"))]
fn stop_within(b: &std::convert::Infallible) -> std::time::Duration {
    match *b {}
}

#[cfg(unix)]
use tokio::signal::unix::*;
use etcds::cli::{EtcdCliArgs, EtcdConfig};
use etcds::cluster::EtcdNode;
use etcds::message;

#[cfg(unix)]
async fn wait_for_signal() -> Result<String, String> {
    let mut int_stream = signal(SignalKind::interrupt()).map_err(|e| message("main.signal", &[("err", &e.to_string())]))?;
    let mut term_stream = signal(SignalKind::terminate()).map_err(|e| message("main.signal", &[("err", &e.to_string())]))?;
    let mut hangup_stream = signal(SignalKind::hangup()).map_err(|e| message("main.signal", &[("err", &e.to_string())]))?;
    let sig = tokio::select! {
		_ = int_stream.recv() => {"interrupt"},
		_ = term_stream.recv() => {"terminate"},
		_ = hangup_stream.recv() => {"hangup"},
	};
    Ok(sig.to_string())
}

#[cfg(windows)]
async fn wait_for_signal() -> Result<String, String> {
    tokio::signal::ctrl_c().await
        .map(|_| message("main.ctrl_c", &[]))
        .map_err(|err| message("main.signal", &[("err", &err.to_string())]))
}

pub fn exit_with_msg(log: Logger, msg: String, code: u8) -> ExitCode {
    #[cfg(test)]
    assert!(false, "\n{}", msg);

    crit!(log, "\n{}", msg);
    // std::io::stdout().flush().unwrap();
    ExitCode::from(code)
}

pub fn logger(cfg: &EtcdConfig) -> Logger {

    let mut builder = TerminalLoggerBuilder::new();
    builder.channel_size(10240);
    builder.destination(Destination::Stdout);

    #[cfg(debug_assertions)]
    builder.source_location(SourceLocation::LocalFileAndLine);

    #[cfg(not(debug_assertions))]
    builder.source_location(SourceLocation::None);

    builder.level(severity_from_string(&cfg.log_level));
    builder.overflow_strategy(sloggers::types::OverflowStrategy::Drop);
    builder.build().unwrap()
}
#[inline]
fn severity_from_string(severity: &String) -> Severity {
    match severity.to_lowercase().as_str() {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        "info" => Severity::Info,
        "debug" => Severity::Debug,
        "trace" => Severity::Trace,
        _ => Severity::Critical,
    }
}

