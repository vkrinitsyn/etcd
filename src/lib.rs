use slog::Logger;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;
use tonic::{async_trait, Request, Response, Status};
use tonic::transport::{Error, Server};
use crate::cli::Config;
use crate::cluster::{ EtcdNode};
use crate::etcdpb::etcdserverpb::kv_server::Kv;
use crate::etcdpb::etcdserverpb::{DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse, TxnRequest, TxnResponse};
use crate::srv::parking::*;
use crate::etcdpb::v3lockpb::lock_server::{Lock, LockServer};
use crate::etcdpb::v3lockpb::{LockRequest, LockResponse, UnlockRequest, UnlockResponse};
use crate::srv::UNIMPL;

pub mod etcdpb;
pub mod cli;
pub mod cluster;
mod srv;
mod queue;
mod kv;
mod peer;
// mod peer;

//mod iputil;

/// Etcd reconfiguration and integration events
#[derive(Clone)]
pub enum EtcdEvents {
    /// Cache modification event
    Data(KvEvent),
    /// Node management event
    Mgmt(EtcdMgmtEvent),
}

#[derive(Clone)]
pub enum EtcdMgmtEvent {
    /// runtime reconfigure event
    Config(Config),
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

