/// Convert internal pair format (BTC/USD) to Coinbase format (BTC-USD).
pub fn to_coinbase(symbol: &str) -> String {
    symbol.replace('/', "-")
}

/// Convert Coinbase pair format (BTC-USD) to internal format (BTC/USD).
pub fn to_internal(product_id: &str) -> String {
    product_id.replace('-', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_coinbase() {
        assert_eq!(to_coinbase("BTC/USD"), "BTC-USD");
        assert_eq!(to_coinbase("ETH/USD"), "ETH-USD");
        assert_eq!(to_coinbase("DOGE/USD"), "DOGE-USD");
    }

    #[test]
    fn test_to_internal() {
        assert_eq!(to_internal("BTC-USD"), "BTC/USD");
        assert_eq!(to_internal("ETH-USD"), "ETH/USD");
        assert_eq!(to_internal("DOGE-USD"), "DOGE/USD");
    }

    #[test]
    fn test_roundtrip() {
        let original = "SOL/USD";
        assert_eq!(to_internal(&to_coinbase(original)), original);
    }
}
