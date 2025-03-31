

#[macro_use]
extern crate slog;

use i18n_embed::{
    fluent::{fluent_language_loader, FluentLanguageLoader},
    LanguageLoader,
};
use rust_embed::RustEmbed;
use etcd::cli::Config;


use std::{fs::File, io::BufReader};

use clap::{Parser,  Command};
use std::{
    error::Error,
    path::PathBuf,
};

#[derive(RustEmbed)]
#[folder = "i18n/"]
struct Localizations;

use clap_serde_derive::ClapSerde;
use lazy_static::lazy_static;
use slog::Logger;
use sloggers::Build;
use sloggers::terminal::{Destination, TerminalLoggerBuilder};
use sloggers::types::{Severity, SourceLocation};
use etcd::cluster::{ EtcdNode};

lazy_static! {
    static ref LANGUAGE_LOADER: FluentLanguageLoader = {
		let loader: FluentLanguageLoader = fluent_language_loader!();
		loader
		.load_languages(&Localizations, &[loader.fallback_language().to_owned()])
		.unwrap();
		loader
    };
}

#[macro_export]
macro_rules! fl {
    ($message_id:literal) => {{
        i18n_embed_fl::fl!($crate::LANGUAGE_LOADER, $message_id)
    }};

    ($message_id:literal, $($args:expr),*) => {{
        i18n_embed_fl::fl!($crate::LANGUAGE_LOADER, $message_id, $($args), *)
    }};
}


#[tokio::main]
async fn main() -> Result<(), String> {
    // Parse whole args with clap
    let mut args = Args::parse();
    
    // Get config file
    let config = if let Ok(f) = File::open(&args.config_path) {
        // Parse config with serde
        match serde_yaml::from_reader::<_, <Config as ClapSerde>::Opt>(BufReader::new(f)) {
            // merge config already parsed from clap
            Ok(config) => Config::from(config).merge(&mut args.config),
            Err(err) => panic!("Error in configuration file:\n{}", err),
        }
    } else {
        // If there is not config file return only config parsed from clap
        Config::from(&mut args.config)
    };
    let log = logger(&config);
    let c = EtcdNode::init(config, log.clone()).await?;
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

pub fn logger(cfg: &Config) -> Logger {

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

#[derive(Parser)]
// author, version, about,
#[command(after_help = "https://etcd.io/docs/v3.5/op-guide/configuration/")]
struct Args {
    /// Input files
    input: Vec<std::path::PathBuf>,

    /// Config file
    #[arg(long = "config", default_value = "config.yml")]
    config_path: std::path::PathBuf,

    /// Rest of arguments
    #[command(flatten)]
    pub config: <Config as ClapSerde>::Opt,
}

