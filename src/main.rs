

#[macro_use]
extern crate slog;

use etcd::cli::{EtcdCliArgs, EtcdConfig};
use std::{fs::File, io::BufReader};
use clap::Parser;
use slog::Logger;
use sloggers::Build;
use sloggers::terminal::{Destination, TerminalLoggerBuilder};
use sloggers::types::{Severity, SourceLocation};
use etcd::cluster::EtcdNode;

/// etcd standalone entry point
#[tokio::main]
async fn main() -> Result<(), String> {
    // Parse whole args with clap
    let mut args = EtcdCliArgs::parse();
   
    // Get config file
    let config = if let Ok(f) = File::open(&args.config_path) {
        args.parse_from(BufReader::new(f))?
    } else {
        // If there is not config file return only config parsed from clap
        EtcdConfig::from(&mut args.config)
    };
    let log = logger(&config);
    let c = EtcdNode::init(
        config, log.clone(),
        #[cfg(feature = "tracer")] None,
    ).await?;
    let _ = c.serve().await?;

    match wait_for_signal().await {
        Ok(msg) => {
            exit_with_msg(log, msg, 0);
        }
        Err(msg) => {
            exit_with_msg(log, msg, 10);
        }
    }
}

#[cfg(unix)]
use tokio::signal::unix::*;

#[cfg(unix)]
async fn wait_for_signal() -> Result<String, String> {
    let mut int_stream = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    let mut term_stream = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut hangup_stream = signal(SignalKind::hangup()).map_err(|e| e.to_string())?;
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
        .map(|_| format!("got CTRL-C."))
        .map_err(|err| format!("Unable to listen for shutdown signal: {}", err))
}

pub fn exit_with_msg(log: Logger, msg: String, code: i32) -> ! {
    #[cfg(test)]
    assert!(false, "\n{}", msg);

    crit!(log, "\n{}", msg);
    // std::io::stdout().flush().unwrap();
    std::process::exit(code);
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

