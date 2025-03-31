use crate::cli::Config;
use crate::etcdpb::etcdserverpb::{DeleteRangeRequest, PutRequest, TxnRequest};

pub mod etcdpb;
pub mod cli;
pub mod cluster;
mod srv;
mod queue;
mod kv;
mod peer;

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

