use rust_decimal::Decimal;
use std::collections::HashMap;
use trading_primitives::Ticker;
use crate::config::RiskConfig;
use crate::state::bot_state::BotState;
use crate::types::OrderSide;

/// Check if opening a new buy position is allowed by risk limits.
/// Includes pending (unacked + open) buy orders as exposure to prevent
/// over-buying when multiple orders are in flight before fills arrive.
pub fn can_open_buy(
    state: &BotState,
    pair: &Ticker,
    size_usd: Decimal,
    current_price: Decimal,
    prices: &HashMap<Ticker, Decimal>,
    config: &RiskConfig,
) -> bool {
    // Count pending buy orders for this pair as committed exposure
    let pending_pair_buy_exposure: Decimal = state.open_orders.values()
        .filter(|o| &o.pair == pair && o.side == OrderSide::Buy)
        .map(|o| o.price * o.qty)
        .sum();

    let pair_exposure = state.pair_exposure_usd(pair, current_price);
    if pair_exposure + pending_pair_buy_exposure + size_usd > config.max_inventory_usd {
        return false;
    }

    // Count all pending buy orders across all pairs for total exposure check
    let pending_total_buy_exposure: Decimal = state.open_orders.values()
        .filter(|o| o.side == OrderSide::Buy)
        .map(|o| o.price * o.qty)
        .sum();

    let total_exposure = state.total_exposure_usd(prices);
    if total_exposure + pending_total_buy_exposure + size_usd > config.max_total_exposure_usd {
        return false;
    }

    true
}
