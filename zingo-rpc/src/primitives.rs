//! Zingo-RPC primitives

use std::sync::{atomic::AtomicBool, Arc};

pub struct ProxyConfig {
    pub lightwalletd_uri: http::Uri,
    pub zebrad_uri: http::Uri,
    pub online: Arc<AtomicBool>,
}
