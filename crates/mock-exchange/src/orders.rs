use std::collections::HashMap;

/// Side of an order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Side {
    Buy,
    Sell,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Side::Buy => write!(f, "buy"),
            Side::Sell => write!(f, "sell"),
        }
    }
}

/// Order type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OrderType {
    Limit,
    Market,
}

/// A resting or pending order.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Order {
    pub order_id: String,
    pub cl_ord_id: String,
    pub symbol: String,
    pub side: Side,
    pub order_type: OrderType,
    pub price: f64,
    pub qty: f64,
    pub filled_qty: f64,
    pub status: OrderStatus,
    pub post_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum OrderStatus {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
}

impl Order {
    pub fn remaining_qty(&self) -> f64 {
        self.qty - self.filled_qty
    }
}

/// Store for all open orders, keyed by cl_ord_id.
#[derive(Debug)]
#[allow(dead_code)]
pub struct OrderStore {
    pub orders: HashMap<String, Order>,
}

#[allow(dead_code)]
impl OrderStore {
    pub fn new() -> Self {
        Self {
            orders: HashMap::new(),
        }
    }

    pub fn insert(&mut self, order: Order) {
        self.orders.insert(order.cl_ord_id.clone(), order);
    }

    pub fn get(&self, cl_ord_id: &str) -> Option<&Order> {
        self.orders.get(cl_ord_id)
    }

    pub fn get_mut(&mut self, cl_ord_id: &str) -> Option<&mut Order> {
        self.orders.get_mut(cl_ord_id)
    }

    pub fn remove(&mut self, cl_ord_id: &str) -> Option<Order> {
        self.orders.remove(cl_ord_id)
    }

    /// Get all open (non-filled, non-cancelled) orders for a symbol.
    pub fn open_orders_for_symbol(&self, symbol: &str) -> Vec<&Order> {
        self.orders
            .values()
            .filter(|o| {
                o.symbol == symbol
                    && o.status != OrderStatus::Filled
                    && o.status != OrderStatus::Canceled
            })
            .collect()
    }

    /// Get all open orders.
    pub fn all_open_orders(&self) -> Vec<&Order> {
        self.orders
            .values()
            .filter(|o| o.status != OrderStatus::Filled && o.status != OrderStatus::Canceled)
            .collect()
    }

    /// Cancel all open orders. Returns the cl_ord_ids of cancelled orders.
    pub fn cancel_all(&mut self) -> Vec<String> {
        let mut cancelled = Vec::new();
        for order in self.orders.values_mut() {
            if order.status != OrderStatus::Filled && order.status != OrderStatus::Canceled {
                order.status = OrderStatus::Canceled;
                cancelled.push(order.cl_ord_id.clone());
            }
        }
        cancelled
    }
}
