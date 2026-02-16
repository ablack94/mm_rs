use rust_decimal::Decimal;

/// 24h ticker data for a trading pair.
#[derive(Debug, Clone)]
pub struct TickerData {
    pub open: Decimal,
    pub close: Decimal,
    pub volume_24h: Decimal,
    pub change_pct: Decimal,
}
