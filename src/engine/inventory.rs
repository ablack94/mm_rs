use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use crate::state::bot_state::BotState;

/// Compute inventory skew for a pair.
///
/// Returns a value in [-1, 1]:
///   +1 = no position (want to buy)
///   -1 = fully loaded (want to sell)
///
/// Used to shift quotes: positive skew tightens bid, widens ask.
pub fn inventory_skew(
    state: &BotState,
    symbol: &str,
    current_price: Decimal,
    max_inventory_usd: Decimal,
) -> Decimal {
    if max_inventory_usd.is_zero() {
        return Decimal::ONE;
    }
    let exposure = state.pair_exposure_usd(symbol, current_price);
    let skew = Decimal::ONE - (dec!(2) * exposure / max_inventory_usd);
    skew.max(dec!(-1)).min(Decimal::ONE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Position;

    #[test]
    fn test_no_position_skew_is_one() {
        let state = BotState::default();
        let skew = inventory_skew(&state, "TEST/USD", dec!(100), dec!(200));
        assert_eq!(skew, Decimal::ONE);
    }

    #[test]
    fn test_half_inventory_skew_is_zero() {
        let mut state = BotState::default();
        state.positions.insert("TEST/USD".into(), Position {
            qty: dec!(1),
            avg_cost: dec!(100),
        });
        // exposure = 1 * 100 = 100, max = 200
        // skew = 1 - 2*100/200 = 1 - 1 = 0
        let skew = inventory_skew(&state, "TEST/USD", dec!(100), dec!(200));
        assert_eq!(skew, Decimal::ZERO);
    }

    #[test]
    fn test_full_inventory_skew_is_negative_one() {
        let mut state = BotState::default();
        state.positions.insert("TEST/USD".into(), Position {
            qty: dec!(2),
            avg_cost: dec!(100),
        });
        let skew = inventory_skew(&state, "TEST/USD", dec!(100), dec!(200));
        assert_eq!(skew, dec!(-1));
    }
}
