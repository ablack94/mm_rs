pub mod command;
pub mod event;
pub mod traits;
pub mod order_tracking;
pub mod capabilities;

// Re-export key types at crate root.
pub use command::{ProxyCommand, OrderPriority};
pub use event::{ProxyEvent, BookLevel, parse_proxy_event};
pub use traits::{ExchangeClient, OrderManager, DeadManSwitch};
pub use order_tracking::{
    OrderRegistry, FillLedger, PositionTracker,
    TrackedOrder, FillRecord, FillSource, OrderStatus, ProxyPosition,
};
pub use capabilities::ExchangeCapabilities;
