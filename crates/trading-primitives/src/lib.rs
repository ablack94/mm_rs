pub mod order;
pub mod book;
pub mod pair;
pub mod fill;
pub mod position;
pub mod symbol;
pub mod ticker;
pub mod config;
pub mod protocol;
pub mod traits;

// Re-export all public types at crate root for convenience.
pub use order::{OrderSide, OrderRequest};
pub use book::{OrderBook, LevelUpdate, BookLevel, Spread};
pub use pair::PairInfo;
pub use fill::{Fill, TradeRecord};
pub use position::Position;
pub use symbol::{Symbol, Ticker, TickerParseError};
pub use ticker::TickerData;
pub use config::{PairState, PairConfig, GlobalDefaults, ResolvedConfig};
pub use traits::{ExchangeClient, OrderManager};
