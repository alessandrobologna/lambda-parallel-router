use std::collections::{HashSet, VecDeque};

use bytes::Bytes;
use http::HeaderMap;
use tokio::sync::mpsc;
use tokio::sync::{Mutex, Notify};

use crate::lpr::VirtualInvocation;

pub struct ProxyState {
    pub inner: Mutex<InnerState>,
    pub notify: Notify,
}

impl ProxyState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(InnerState::default()),
            notify: Notify::new(),
        }
    }
}

impl Default for ProxyState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
pub struct InnerState {
    pub fetching_outer: bool,
    pub active: ActiveInvocation,
}

pub enum ActiveInvocation {
    None,
    PassThrough(PassThroughInvocation),
    LprBatch(LprBatchInvocation),
    LprFinalizing,
}

impl Default for ActiveInvocation {
    fn default() -> Self {
        Self::None
    }
}

pub struct PassThroughInvocation {
    pub request_id: String,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub delivered: bool,
}

pub struct LprBatchInvocation {
    pub outer_request_id: String,
    pub base_headers: HeaderMap,
    pub queue: VecDeque<VirtualInvocation>,
    pub inflight: HashSet<String>,
    pub stream_tx: Option<mpsc::UnboundedSender<Bytes>>,
    pub stream_join: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}
