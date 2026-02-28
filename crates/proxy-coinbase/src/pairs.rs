/// Convert internal pair format (BTC/USD) to Coinbase format (BTC-USD).
pub fn to_coinbase(symbol: &str) -> String {
    symbol.replace('/', "-")
}

/// Convert Coinbase pair format (BTC-USD) to internal format (BTC/USDC).
/// Coinbase merges USD/USDC order books; we normalize all USD pairs to USDC
/// since that's what we actually trade.
pub fn to_internal(product_id: &str) -> String {
    let symbol = product_id.replace('-', "/");
    // Normalize USD → USDC (but don't double-convert USDC → USDCC)
    if symbol.ends_with("/USD") {
        format!("{}C", symbol)
    } else {
        symbol
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_coinbase() {
        assert_eq!(to_coinbase("BTC/USDC"), "BTC-USDC");
        assert_eq!(to_coinbase("ETH/USDC"), "ETH-USDC");
        assert_eq!(to_coinbase("DOGE/USDC"), "DOGE-USDC");
    }

    #[test]
    fn test_to_internal() {
        // USD pairs normalize to USDC
        assert_eq!(to_internal("BTC-USD"), "BTC/USDC");
        assert_eq!(to_internal("ETH-USD"), "ETH/USDC");
        assert_eq!(to_internal("DOGE-USD"), "DOGE/USDC");
    }

    #[test]
    fn test_to_internal_usdc_passthrough() {
        // USDC pairs stay as USDC (no double conversion)
        assert_eq!(to_internal("DOGE-USDC"), "DOGE/USDC");
        assert_eq!(to_internal("XRP-USDC"), "XRP/USDC");
    }

    #[test]
    fn test_roundtrip() {
        // USDC pairs round-trip correctly
        let original = "SOL/USDC";
        assert_eq!(to_internal(&to_coinbase(original)), original);
    }
}
