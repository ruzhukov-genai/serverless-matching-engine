//! Core matching logic — price-time priority.
//!
//! BUY at $P: match asks where price <= P (lowest ask first, then oldest)
//! SELL at $P: match bids where price >= P (highest bid first, then oldest)
//!
//! Handles: Limit, Market, TIF (GTC/IOC/FOK), STP, partial fills.

use chrono::Utc;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::types::{
    Order, OrderStatus, OrderType, SelfTradePreventionMode, Side, TimeInForce, Trade,
};

/// Result of a match attempt.
pub struct MatchResult {
    /// Trades executed during this match.
    pub trades: Vec<Trade>,
    /// Updated resting orders (with new remaining/status).
    pub book_updates: Vec<Order>,
    /// The incoming order after matching.
    pub incoming: Order,
    /// Orders cancelled due to self-trade prevention.
    pub stp_cancelled: Vec<Order>,
}

/// Match an incoming order against the resting book.
///
/// `book` must be sorted for the *opposite* side:
/// - Incoming Buy:  book contains asks sorted by price ASC, created_at ASC
/// - Incoming Sell: book contains bids sorted by price DESC, created_at ASC
///
/// Returns a `MatchResult` describing the outcome.  The `book` slice is NOT
/// mutated — all changes are reported through `book_updates` and `trades`.
pub fn match_order(incoming: &Order, book: &mut Vec<Order>) -> MatchResult {
    let mut incoming = incoming.clone();
    let mut trades: Vec<Trade> = Vec::new();
    let mut book_updates: Vec<Order> = Vec::new();
    let mut stp_cancelled: Vec<Order> = Vec::new();

    // ── FOK pre-check ─────────────────────────────────────────────────────
    if incoming.tif == TimeInForce::FOK {
        let available: Decimal = book
            .iter()
            .filter(|r| price_matches(&incoming, r))
            .map(|r| r.remaining)
            .fold(Decimal::ZERO, |acc, x| acc + x);
        if available < incoming.remaining {
            incoming.status = OrderStatus::Cancelled;
            return MatchResult {
                trades: vec![],
                book_updates: vec![],
                incoming,
                stp_cancelled: vec![],
            };
        }
    }

    // ── Market orders: treat as IOC with no price restriction ─────────────
    let effective_tif = if incoming.order_type == OrderType::Market {
        TimeInForce::IOC
    } else {
        incoming.tif
    };

    if incoming.order_type == OrderType::Market && incoming.price.is_none() {
        // keep going — no price filter
    }

    // ── Walk book in priority order ───────────────────────────────────────
    for resting in book.iter_mut() {
        if incoming.remaining == Decimal::ZERO {
            break;
        }

        // Price compatibility check
        if !price_matches(&incoming, resting) {
            break; // book is sorted, no further matches possible
        }

        // ── STP check ─────────────────────────────────────────────────────
        if incoming.user_id == resting.user_id
            && incoming.stp_mode != SelfTradePreventionMode::None
        {
            match incoming.stp_mode {
                SelfTradePreventionMode::CancelTaker => {
                    incoming.status = OrderStatus::Cancelled;
                    stp_cancelled.push(incoming.clone());
                    break;
                }
                SelfTradePreventionMode::CancelMaker => {
                    resting.status = OrderStatus::Cancelled;
                    stp_cancelled.push(resting.clone());
                    book_updates.push(resting.clone());
                    continue; // skip this resting order, try the next
                }
                SelfTradePreventionMode::CancelBoth => {
                    incoming.status = OrderStatus::Cancelled;
                    resting.status = OrderStatus::Cancelled;
                    stp_cancelled.push(incoming.clone());
                    stp_cancelled.push(resting.clone());
                    book_updates.push(resting.clone());
                    break;
                }
                SelfTradePreventionMode::None => {}
            }
        }

        // ── Fill calculation ──────────────────────────────────────────────
        let fill_qty = incoming.remaining.min(resting.remaining);
        let fill_price = resting.price.unwrap_or_else(|| {
            incoming.price.unwrap_or(Decimal::ZERO)
        });

        let (buy_order_id, sell_order_id, buyer_id, seller_id) = match incoming.side {
            Side::Buy => (
                incoming.id,
                resting.id,
                incoming.user_id.clone(),
                resting.user_id.clone(),
            ),
            Side::Sell => (
                resting.id,
                incoming.id,
                resting.user_id.clone(),
                incoming.user_id.clone(),
            ),
        };

        let trade = Trade {
            id: Uuid::new_v4(),
            pair_id: incoming.pair_id.clone(),
            buy_order_id,
            sell_order_id,
            buyer_id,
            seller_id,
            price: fill_price,
            quantity: fill_qty,
            sequence: 0, // set by DB
            created_at: Utc::now(),
        };
        trades.push(trade);

        // Update remaining quantities
        incoming.remaining -= fill_qty;
        resting.remaining -= fill_qty;

        // Update statuses
        resting.status = if resting.remaining == Decimal::ZERO {
            OrderStatus::Filled
        } else {
            OrderStatus::PartiallyFilled
        };
        resting.updated_at = Utc::now();
        book_updates.push(resting.clone());

        incoming.status = if incoming.remaining == Decimal::ZERO {
            OrderStatus::Filled
        } else {
            OrderStatus::PartiallyFilled
        };
        incoming.updated_at = Utc::now();
    }

    // ── Apply TIF rules ───────────────────────────────────────────────────
    if incoming.remaining > Decimal::ZERO && incoming.status != OrderStatus::Cancelled {
        match effective_tif {
            TimeInForce::GTC => {
                // Remainder rests in book — leave as New or PartiallyFilled
                if trades.is_empty() {
                    incoming.status = OrderStatus::New;
                }
                // else already PartiallyFilled
            }
            TimeInForce::IOC | TimeInForce::FOK => {
                incoming.status = OrderStatus::Cancelled;
            }
        }
    }

    MatchResult {
        trades,
        book_updates,
        incoming,
        stp_cancelled,
    }
}

/// Check if the incoming order's price is compatible with the resting order.
fn price_matches(incoming: &Order, resting: &Order) -> bool {
    // Market orders match everything
    if incoming.order_type == OrderType::Market {
        return true;
    }
    match (incoming.side, incoming.price, resting.price) {
        (Side::Buy, Some(bid), Some(ask)) => bid >= ask,
        (Side::Sell, Some(ask), Some(bid)) => ask <= bid,
        _ => false, // missing price for limit order — shouldn't happen
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn make_order(
        side: Side,
        order_type: OrderType,
        tif: TimeInForce,
        price: Option<Decimal>,
        qty: Decimal,
        user_id: &str,
    ) -> Order {
        Order {
            id: Uuid::new_v4(),
            user_id: user_id.to_string(),
            pair_id: "BTC-USDT".to_string(),
            side,
            order_type,
            tif,
            price,
            quantity: qty,
            remaining: qty,
            status: OrderStatus::New,
            stp_mode: SelfTradePreventionMode::None,
            version: 1,
            sequence: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn limit_buy(price: Decimal, qty: Decimal) -> Order {
        make_order(Side::Buy, OrderType::Limit, TimeInForce::GTC, Some(price), qty, "user-1")
    }

    fn limit_sell(price: Decimal, qty: Decimal) -> Order {
        make_order(Side::Sell, OrderType::Limit, TimeInForce::GTC, Some(price), qty, "user-2")
    }

    #[test]
    fn test_two_crossing_limits() {
        let incoming = limit_buy(dec!(100), dec!(1));
        let mut book = vec![limit_sell(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].price, dec!(100));
        assert_eq!(result.trades[0].quantity, dec!(1));
        assert_eq!(result.incoming.status, OrderStatus::Filled);
    }

    #[test]
    fn test_partial_fill() {
        let incoming = limit_buy(dec!(100), dec!(2));
        let mut book = vec![limit_sell(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].quantity, dec!(1));
        assert_eq!(result.incoming.remaining, dec!(1));
        assert_eq!(result.incoming.status, OrderStatus::PartiallyFilled);
    }

    #[test]
    fn test_market_order() {
        let incoming = make_order(
            Side::Buy, OrderType::Market, TimeInForce::IOC,
            None, dec!(1), "user-1",
        );
        let mut book = vec![limit_sell(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].price, dec!(100));
        assert_eq!(result.incoming.status, OrderStatus::Filled);
    }

    #[test]
    fn test_fok_insufficient() {
        let incoming = make_order(
            Side::Buy, OrderType::Limit, TimeInForce::FOK,
            Some(dec!(100)), dec!(2), "user-1",
        );
        let mut book = vec![limit_sell(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 0);
        assert_eq!(result.incoming.status, OrderStatus::Cancelled);
    }

    #[test]
    fn test_ioc_partial() {
        let incoming = make_order(
            Side::Buy, OrderType::Limit, TimeInForce::IOC,
            Some(dec!(100)), dec!(2), "user-1",
        );
        let mut book = vec![limit_sell(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].quantity, dec!(1));
        assert_eq!(result.incoming.status, OrderStatus::Cancelled);
    }

    #[test]
    fn test_stp_cancel_taker() {
        let mut incoming = make_order(
            Side::Buy, OrderType::Limit, TimeInForce::GTC,
            Some(dec!(100)), dec!(1), "user-1",
        );
        incoming.stp_mode = SelfTradePreventionMode::CancelTaker;
        let mut book = vec![make_order(
            Side::Sell, OrderType::Limit, TimeInForce::GTC,
            Some(dec!(100)), dec!(1), "user-1", // same user
        )];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 0);
        assert_eq!(result.incoming.status, OrderStatus::Cancelled);
        assert_eq!(result.stp_cancelled.len(), 1);
    }

    #[test]
    fn test_stp_cancel_maker() {
        let mut incoming = make_order(
            Side::Buy, OrderType::Limit, TimeInForce::GTC,
            Some(dec!(100)), dec!(1), "user-1",
        );
        incoming.stp_mode = SelfTradePreventionMode::CancelMaker;
        let resting = make_order(
            Side::Sell, OrderType::Limit, TimeInForce::GTC,
            Some(dec!(100)), dec!(1), "user-1", // same user
        );
        let mut book = vec![resting];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 0);
        assert_eq!(result.stp_cancelled.len(), 1);
        assert_eq!(result.stp_cancelled[0].status, OrderStatus::Cancelled);
    }

    #[test]
    fn test_price_time_priority() {
        // 3 asks at same price, different created_at
        let t0 = Utc::now();
        let mut ask1 = limit_sell(dec!(100), dec!(1));
        ask1.created_at = t0;
        let mut ask2 = limit_sell(dec!(100), dec!(1));
        ask2.created_at = t0 + chrono::Duration::milliseconds(100);
        let mut ask3 = limit_sell(dec!(100), dec!(1));
        ask3.created_at = t0 + chrono::Duration::milliseconds(200);

        // book is pre-sorted: price ASC, then time ASC
        let mut book = vec![ask1.clone(), ask2.clone(), ask3.clone()];

        let incoming = limit_buy(dec!(100), dec!(1));
        let result = match_order(&incoming, &mut book);

        assert_eq!(result.trades.len(), 1);
        // Should have matched ask1 (earliest)
        assert_eq!(result.trades[0].sell_order_id, ask1.id);
        assert_eq!(result.incoming.status, OrderStatus::Filled);
    }

    #[test]
    fn test_multi_level_fill() {
        // Buyer wants 3, asks at 99, 100, 101
        let ask1 = limit_sell(dec!(99), dec!(1));
        let ask2 = limit_sell(dec!(100), dec!(1));
        let ask3 = limit_sell(dec!(101), dec!(1));
        let mut book = vec![ask1, ask2, ask3]; // sorted price ASC

        let incoming = limit_buy(dec!(100), dec!(3));
        let result = match_order(&incoming, &mut book);

        // Should fill 99 and 100 (not 101 > 100)
        assert_eq!(result.trades.len(), 2);
        assert_eq!(result.incoming.remaining, dec!(1));
    }

    #[test]
    fn test_empty_book_limit() {
        let incoming = limit_buy(dec!(100), dec!(1));
        let mut book = vec![];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 0);
        assert_eq!(result.incoming.status, OrderStatus::New);
    }

    #[test]
    fn test_empty_book_market() {
        let incoming = make_order(
            Side::Buy, OrderType::Market, TimeInForce::IOC,
            None, dec!(1), "user-1",
        );
        let mut book = vec![];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 0);
        // Market = IOC, unfilled remainder cancelled
        assert_eq!(result.incoming.status, OrderStatus::Cancelled);
    }
}
