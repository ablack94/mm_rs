use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// Symbol — a single currency identifier (e.g., "BTC", "USD")
// ---------------------------------------------------------------------------

/// A single currency identifier (e.g., "BTC", "USD").
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Symbol(String);

impl Symbol {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for Symbol {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Symbol(s.to_string()))
    }
}

impl From<&str> for Symbol {
    fn from(s: &str) -> Self {
        Symbol(s.to_string())
    }
}

impl From<String> for Symbol {
    fn from(s: String) -> Self {
        Symbol(s)
    }
}

impl AsRef<str> for Symbol {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Ticker — a trading pair: base/quote (e.g., BTC/USD)
// ---------------------------------------------------------------------------

/// A trading pair: base/quote (e.g., BTC/USD).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ticker {
    base: Symbol,
    quote: Symbol,
}

/// Error returned when parsing a ticker string fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickerParseError(String);

impl fmt::Display for TickerParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid ticker: {}", self.0)
    }
}

impl std::error::Error for TickerParseError {}

impl Ticker {
    pub fn new(base: Symbol, quote: Symbol) -> Self {
        Ticker { base, quote }
    }

    pub fn base(&self) -> &Symbol {
        &self.base
    }

    pub fn quote(&self) -> &Symbol {
        &self.quote
    }
}

impl fmt::Display for Ticker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.base, self.quote)
    }
}

impl std::str::FromStr for Ticker {
    type Err = TickerParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once('/') {
            Some((base, quote)) if !base.is_empty() && !quote.is_empty() => {
                Ok(Ticker {
                    base: Symbol::from(base),
                    quote: Symbol::from(quote),
                })
            }
            _ => Err(TickerParseError(s.to_string())),
        }
    }
}

impl From<&str> for Ticker {
    fn from(s: &str) -> Self {
        s.parse().unwrap_or_else(|e: TickerParseError| panic!("{}", e))
    }
}

impl From<String> for Ticker {
    fn from(s: String) -> Self {
        Ticker::from(s.as_str())
    }
}

impl Serialize for Ticker {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Ticker {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_display_and_from() {
        let s = Symbol::from("BTC");
        assert_eq!(s.to_string(), "BTC");
        assert_eq!(s.as_str(), "BTC");
        assert_eq!(s.as_ref(), "BTC");
    }

    #[test]
    fn symbol_parse() {
        let s: Symbol = "USD".parse().unwrap();
        assert_eq!(s, Symbol::from("USD"));
    }

    #[test]
    fn symbol_serde_roundtrip() {
        let s = Symbol::from("ETH");
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#""ETH""#);
        let parsed: Symbol = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn ticker_new_and_accessors() {
        let t = Ticker::new(Symbol::from("BTC"), Symbol::from("USD"));
        assert_eq!(t.base(), &Symbol::from("BTC"));
        assert_eq!(t.quote(), &Symbol::from("USD"));
    }

    #[test]
    fn ticker_display() {
        let t = Ticker::from("BTC/USD");
        assert_eq!(t.to_string(), "BTC/USD");
    }

    #[test]
    fn ticker_parse() {
        let t: Ticker = "ETH/USD".parse().unwrap();
        assert_eq!(t.base(), &Symbol::from("ETH"));
        assert_eq!(t.quote(), &Symbol::from("USD"));
    }

    #[test]
    fn ticker_parse_error() {
        assert!("BTCUSD".parse::<Ticker>().is_err());
        assert!("/USD".parse::<Ticker>().is_err());
        assert!("BTC/".parse::<Ticker>().is_err());
        assert!("".parse::<Ticker>().is_err());
    }

    #[test]
    fn ticker_from_string() {
        let t = Ticker::from("CAMP/USD".to_string());
        assert_eq!(t.to_string(), "CAMP/USD");
    }

    #[test]
    fn ticker_serde_roundtrip() {
        let t = Ticker::from("BTC/USD");
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, r#""BTC/USD""#);
        let parsed: Ticker = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, t);
    }

    #[test]
    fn ticker_as_hashmap_key_serde() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        map.insert(Ticker::from("BTC/USD"), 42);
        map.insert(Ticker::from("ETH/USD"), 99);

        let json = serde_json::to_string(&map).unwrap();
        let parsed: HashMap<Ticker, i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[&Ticker::from("BTC/USD")], 42);
        assert_eq!(parsed[&Ticker::from("ETH/USD")], 99);
    }

    #[test]
    fn ticker_eq_and_hash() {
        use std::collections::HashSet;
        let a = Ticker::from("BTC/USD");
        let b: Ticker = "BTC/USD".parse().unwrap();
        assert_eq!(a, b);

        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }

    #[test]
    fn ticker_ord() {
        let btc = Ticker::from("BTC/USD");
        let eth = Ticker::from("ETH/USD");
        assert!(btc < eth);
    }
}
