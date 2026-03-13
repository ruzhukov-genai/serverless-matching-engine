//! Core matching logic — price-time priority.
//!
//! BUY at $P: match asks where price <= P (lowest first, then oldest)
//! SELL at $P: match bids where price >= P (highest first, then oldest)
//!
//! Handles: Limit orders, Market orders, TIF (GTC/IOC/FOK),
//! Self-Trade Prevention, partial fills.

use sme_shared::{Order, Trade, Side, OrderType, TimeInForce, OrderStatus, SelfTradePreventionMode};

/// Result of a match attempt.
pub struct MatchResult {
    /// Trades executed during this match.
    pub trades: Vec<Trade>,
    /// Updated resting orders (modified quantities / removed).
    pub book_updates: Vec<Order>,
    /// The incoming order after matching (may be partially filled or fully filled).
    pub incoming: Order,
    /// Orders canceled due to self-trade prevention.
    pub stp_cancelled: Vec<Order>,
}

/// Match an incoming order against the resting book.
///
/// `book` must be sorted by price-time priority:
/// - For Buy incoming: asks sorted by price ASC, then created_at ASC
/// - For Sell incoming: bids sorted by price DESC, then created_at ASC
pub fn match_order(incoming: &Order, book: &mut Vec<Order>) -> MatchResult {
    // TODO: implement
    //
    // 1. For each resting order in book (price-time priority):
    //    a. Check self-trade prevention (same user_id)
    //    b. Check price compatibility:
    //       - Buy limit:  ask.price <= incoming.price
    //       - Sell limit: bid.price >= incoming.price
    //       - Market: always compatible (no price check)
    //    c. Calculate fill quantity: min(incoming.remaining, resting.remaining)
    //    d. Create Trade
    //    e. Update remaining quantities
    //    f. If incoming fully filled, stop
    //
    // 2. Apply TIF rules:
    //    - GTC: unfilled remainder rests in book
    //    - IOC: cancel unfilled remainder
    //    - FOK: if not fully filled, cancel entire order (rollback trades)
    //
    // 3. Market orders: always IOC behavior (no resting)

    MatchResult {
        trades: vec![],
        book_updates: vec![],
        incoming: incoming.clone(),
        stp_cancelled: vec![],
    }
}
