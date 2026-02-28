use rust_decimal::Decimal;
use std::collections::HashMap;
use trading_primitives::Ticker;
use crate::config::RiskConfig;
use crate::state::bot_state::BotState;

/// Check if opening a new buy position is allowed by risk limits.
pub fn can_open_buy(
    state: &BotState,
    pair: &Ticker,
    size_usd: Decimal,
    current_price: Decimal,
    prices: &HashMap<Ticker, Decimal>,
    config: &RiskConfig,
) -> bool {
    let pair_exposure = state.pair_exposure_usd(pair, current_price);
    if pair_exposure + size_usd > config.max_inventory_usd {
        return false;
    }

    let total_exposure = state.total_exposure_usd(prices);
    if total_exposure + size_usd > config.max_total_exposure_usd {
        return false;
    }

    true
}
