pub mod trade_logger;
pub mod event_source;
pub mod clock;

// Re-export traits from exchange-api.
pub use exchange_api::{ExchangeClient, OrderManager, DeadManSwitch};
pub use trade_logger::TradeLogger;
pub use event_source::EventSource;
pub use clock::{Clock, SystemClock};
