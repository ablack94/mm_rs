use std::time::Instant;

/// Configuration for the token bucket rate limiter.
#[derive(Debug, Clone)]
pub struct TokenBucketConfig {
    /// Bucket capacity (max tokens).
    pub max_tokens: f64,
    /// Tokens restored per second.
    pub refill_rate: f64,
    /// Cost for add_order / amend_order.
    pub order_cost: f64,
    /// Cost for cancel_order.
    pub cancel_cost: f64,
}

impl Default for TokenBucketConfig {
    fn default() -> Self {
        Self {
            max_tokens: 40.0,
            refill_rate: 1.0,
            order_cost: 2.0,
            cancel_cost: 1.0,
        }
    }
}

/// Token bucket rate limiter.
/// Starts full, deducts on each request, refills over time.
pub struct TokenBucket {
    tokens: f64,
    config: TokenBucketConfig,
    last_refill: Instant,
}

impl TokenBucket {
    pub fn new(config: TokenBucketConfig) -> Self {
        let tokens = config.max_tokens;
        Self {
            tokens,
            config,
            last_refill: Instant::now(),
        }
    }

    /// Try to consume tokens for the given WS method.
    /// Returns true (and deducts) if enough tokens are available, false otherwise.
    pub fn try_consume(&mut self, method: &str) -> bool {
        self.refill();
        let cost = self.cost_for(method);
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }

    /// Current tokens remaining (for logging/diagnostics).
    pub fn tokens_remaining(&self) -> f64 {
        self.tokens
    }

    fn cost_for(&self, method: &str) -> f64 {
        match method {
            "add_order" | "amend_order" => self.config.order_cost,
            "cancel_order" => self.config.cancel_cost,
            _ => 0.0,
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * self.config.refill_rate).min(self.config.max_tokens);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn test_config() -> TokenBucketConfig {
        TokenBucketConfig {
            max_tokens: 10.0,
            refill_rate: 1.0,
            order_cost: 2.0,
            cancel_cost: 1.0,
        }
    }

    #[test]
    fn test_initial_bucket_full() {
        let bucket = TokenBucket::new(test_config());
        assert_eq!(bucket.tokens_remaining(), 10.0);
    }

    #[test]
    fn test_consume_deducts_tokens() {
        let mut bucket = TokenBucket::new(test_config());
        assert!(bucket.try_consume("add_order"));
        assert!(bucket.tokens_remaining() <= 8.01);
        assert!(bucket.tokens_remaining() >= 7.99);
    }

    #[test]
    fn test_denied_when_empty() {
        let mut bucket = TokenBucket::new(TokenBucketConfig {
            max_tokens: 1.5,
            refill_rate: 0.0,
            order_cost: 2.0,
            cancel_cost: 1.0,
        });
        assert!(!bucket.try_consume("add_order"));
        assert!(bucket.try_consume("cancel_order"));
    }

    #[test]
    fn test_refill_over_time() {
        let mut bucket = TokenBucket::new(TokenBucketConfig {
            max_tokens: 10.0,
            refill_rate: 2.0,
            order_cost: 2.0,
            cancel_cost: 1.0,
        });
        while bucket.try_consume("cancel_order") {}
        assert!(bucket.tokens_remaining() < 1.0);

        bucket.last_refill = Instant::now() - Duration::from_secs(3);
        assert!(bucket.try_consume("add_order"));
        assert!(bucket.tokens_remaining() >= 3.5);
        assert!(bucket.tokens_remaining() <= 4.5);
    }

    #[test]
    fn test_refill_capped_at_max() {
        let mut bucket = TokenBucket::new(TokenBucketConfig {
            max_tokens: 10.0,
            refill_rate: 100.0,
            order_cost: 2.0,
            cancel_cost: 1.0,
        });
        bucket.try_consume("add_order");
        bucket.last_refill = Instant::now() - Duration::from_secs(60);
        bucket.try_consume("cancel_order");
        assert!(bucket.tokens_remaining() <= 10.0);
        assert!(bucket.tokens_remaining() >= 8.5);
    }

    #[test]
    fn test_method_costs() {
        let config = test_config();
        let mut bucket = TokenBucket::new(config);

        let before = bucket.tokens_remaining();
        bucket.try_consume("add_order");
        let after_add = bucket.tokens_remaining();
        assert!((before - after_add - 2.0).abs() < 0.1);

        let before_amend = bucket.tokens_remaining();
        bucket.try_consume("amend_order");
        let after_amend = bucket.tokens_remaining();
        assert!((before_amend - after_amend - 2.0).abs() < 0.1);

        let before_cancel = bucket.tokens_remaining();
        bucket.try_consume("cancel_order");
        let after_cancel = bucket.tokens_remaining();
        assert!((before_cancel - after_cancel - 1.0).abs() < 0.1);
    }
}
