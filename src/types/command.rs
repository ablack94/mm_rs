use rust_decimal::Decimal;
use super::order::OrderRequest;
use super::fill::TradeRecord;
use crate::state::bot_state::BotState;

/// Commands flowing out of the engine. Each command is a side-effect
/// that the command dispatcher routes to the appropriate handler.
#[derive(Debug, Clone)]
pub enum EngineCommand {
    PlaceOrder(OrderRequest),
    AmendOrder {
        cl_ord_id: String,
        new_price: Option<Decimal>,
        new_qty: Option<Decimal>,
    },
    CancelOrders(Vec<String>),
    CancelAll,
    RefreshDms,
    PersistState(BotState),
    LogTrade(TradeRecord),
    Shutdown {
        reason: String,
    },
}
