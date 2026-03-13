//! Core matching logic — price-time priority.
//!
//! BUY at $P: match asks where price <= P (lowest first, then oldest)
//! SELL at $P: match bids where price >= P (highest first, then oldest)

use sme_shared::{Order, Trade, Side};

/// Match an incoming order against the resting book.
/// Returns a list of trades and the modified resting orders.
pub fn match_order(_incoming: &Order, _book: &mut Vec<Order>) -> Vec<Trade> {
    // TODO: implement price-time priority matching
    vec![]
}
