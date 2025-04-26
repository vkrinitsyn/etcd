
use i18n_embed::{
    fluent::{fluent_language_loader, FluentLanguageLoader},
    LanguageLoader,
};
use rust_embed::RustEmbed;
use lazy_static::lazy_static;
use tokio::sync::mpsc::Sender;
use tonic::Status;
use crate::cli::EtcdConfig;
use crate::etcdpb::etcdserverpb::{DeleteRangeRequest, PutRequest, TxnRequest, WatchResponse};

pub mod etcdpb;
pub mod cli;
pub mod cluster;
mod srv;
pub mod queue;
pub mod kv;
mod peer;

#[derive(RustEmbed)]
#[folder = "i18n/"]
pub struct Localizations;

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

pub type WatchSender = Sender<Result<WatchResponse, Status>>;

/// Etcd reconfiguration and integration events
#[derive(Clone)]
pub enum EtcdEvents {
    /// Cache modification event
    Data(KvEvent),
    /// Node management event
    Mgmt(EtcdMgmtEvent),
}

#[derive(Clone, Debug)]
pub enum EtcdMgmtEvent {
    /// runtime reconfigure event
    Config(EtcdConfig),
    /// change tracer runtime
    #[cfg(feature = "tracer")]
    Tracer(Option<opentelemetry_sdk::trace::SdkTracer>),
    /// Node management event to stop 
    Stop,
    /// Node management event to soft restart 
    Restart,
    /// Node management event to pause(sec)  
    Pause(u16)
}

#[derive(Clone)]
pub enum KvEvent {
    Put(PutRequest),
    Delete(DeleteRangeRequest),
    Txn(TxnRequest),
}

