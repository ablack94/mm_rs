//! Exchange capabilities reported by proxies.
//!
//! Each proxy exposes `GET /capabilities` returning this struct.
//! The bot queries it at startup and skips unsupported features.

use serde::{Deserialize, Serialize};

/// Capabilities supported by an exchange proxy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExchangeCapabilities {
    /// Whether the exchange supports dead man's switch (cancel-all-after).
    #[serde(default)]
    pub dead_man_switch: bool,

    /// Whether the exchange supports order amends (edit-in-place).
    #[serde(default)]
    pub amend_orders: bool,

    /// Whether the exchange supports post-only orders.
    #[serde(default)]
    pub post_only: bool,
}
