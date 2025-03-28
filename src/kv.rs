use std::collections::HashMap;
use std::sync::Arc;
use clap_serde_derive::ClapSerde;
use tokio::sync::mpsc::Receiver;
use tokio::sync::RwLock;
use tokio::time::Instant;
use crate::cli::Config;
use crate::EtcdEvents;
use crate::etcdpb::etcdserverpb::{PutRequest, RangeResponse};
use crate::etcdpb::mvccpb::KeyValue;

/// Key Value
#[derive(Clone)]
pub struct Kv {
  pub key: Vec<u8>,
  pub value: Vec<u8>,
 
  pub create_revision: i64,
  pub mod_revision: i64,
  pub lease: i64,
  
  pub version: u32,
  pub created: Instant,
  pub lut: Instant,
  pub lus: Instant,
}

impl From<PutRequest> for Kv {
  fn from(value: PutRequest) -> Self {
    Kv {
     key: value.key,
     value: value.value,
     create_revision: 0,
     mod_revision: 0,
     lease: 0,
     version: 0,
     created: Instant::now(),
     lut: Instant::now(),
     lus: Instant::now(),
    }
  }
}


impl From<Kv> for KeyValue {
  fn from(value: Kv) -> Self {
   crate::etcdpb::mvccpb::KeyValue {
    key: value.key,
    create_revision: value.create_revision,
    mod_revision: value.mod_revision,
    version: value.version as i64,
    value: value.value,
    lease: value.lease,
   }
  }
}

impl From<&Kv> for KeyValue {
  fn from(value: &Kv) -> Self {
   crate::etcdpb::mvccpb::KeyValue {
    key: value.key.clone(),
    create_revision: value.create_revision,
    mod_revision: value.mod_revision,
    version: value.version as i64,
    value: value.value.clone(),
    lease: value.lease,
   }
  }
}


