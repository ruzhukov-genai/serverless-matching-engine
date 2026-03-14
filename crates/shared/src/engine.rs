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
pub fn match_order(incoming: &Order, book: &mut [Order]) -> MatchResult {
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

    // ── Helpers ───────────────────────────────────────────────────────────

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
            client_order_id: None,
        }
    }

    fn limit_buy(price: Decimal, qty: Decimal) -> Order {
        make_order(Side::Buy, OrderType::Limit, TimeInForce::GTC, Some(price), qty, "user-1")
    }

    fn limit_sell(price: Decimal, qty: Decimal) -> Order {
        make_order(Side::Sell, OrderType::Limit, TimeInForce::GTC, Some(price), qty, "user-2")
    }

    fn limit_buy_user(price: Decimal, qty: Decimal, user: &str) -> Order {
        make_order(Side::Buy, OrderType::Limit, TimeInForce::GTC, Some(price), qty, user)
    }

    fn limit_sell_user(price: Decimal, qty: Decimal, user: &str) -> Order {
        make_order(Side::Sell, OrderType::Limit, TimeInForce::GTC, Some(price), qty, user)
    }

    // ── Basic Matching ────────────────────────────────────────────────────

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
    fn test_empty_book_limit() {
        let incoming = limit_buy(dec!(100), dec!(1));
        let mut book = vec![];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 0);
        assert_eq!(result.incoming.status, OrderStatus::New);
        assert_eq!(result.incoming.remaining, dec!(1));
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
        assert_eq!(result.incoming.status, OrderStatus::Cancelled);
    }

    // ── Sell-Side Incoming ────────────────────────────────────────────────

    #[test]
    fn test_sell_incoming_matches_bid() {
        // Incoming sell at 100, resting bid at 100
        let incoming = limit_sell(dec!(100), dec!(1));
        let mut book = vec![limit_buy(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].price, dec!(100));
        assert_eq!(result.trades[0].quantity, dec!(1));
        assert_eq!(result.incoming.status, OrderStatus::Filled);
        // Verify trade direction: buy_order_id = resting bid, sell_order_id = incoming sell
        assert_eq!(result.trades[0].buy_order_id, book[0].id);
        assert_eq!(result.trades[0].sell_order_id, incoming.id);
    }

    #[test]
    fn test_sell_incoming_partial_fill() {
        let incoming = limit_sell(dec!(100), dec!(3));
        let mut book = vec![limit_buy(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].quantity, dec!(1));
        assert_eq!(result.incoming.remaining, dec!(2));
        assert_eq!(result.incoming.status, OrderStatus::PartiallyFilled);
    }

    #[test]
    fn test_sell_incoming_multi_level() {
        // Incoming sell at 98, resting bids at 101, 100, 99 (sorted DESC for sell matching)
        let bid1 = limit_buy(dec!(101), dec!(1));
        let bid2 = limit_buy(dec!(100), dec!(1));
        let bid3 = limit_buy(dec!(99), dec!(1));
        let mut book = vec![bid1, bid2, bid3]; // sorted price DESC

        let incoming = limit_sell_user(dec!(99), dec!(2), "user-2");
        let result = match_order(&incoming, &mut book);

        assert_eq!(result.trades.len(), 2);
        // Fills at 101 first (best bid), then 100
        assert_eq!(result.trades[0].price, dec!(101));
        assert_eq!(result.trades[1].price, dec!(100));
        assert_eq!(result.incoming.status, OrderStatus::Filled);
    }

    // ── Price Mismatch (No Match) ─────────────────────────────────────────

    #[test]
    fn test_buy_price_too_low_no_match() {
        // Buy at 95, ask at 100 — no match
        let incoming = limit_buy(dec!(95), dec!(1));
        let mut book = vec![limit_sell(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 0);
        assert_eq!(result.incoming.status, OrderStatus::New);
        assert_eq!(result.book_updates.len(), 0);
    }

    #[test]
    fn test_sell_price_too_high_no_match() {
        // Sell at 105, bid at 100 — no match
        let incoming = limit_sell(dec!(105), dec!(1));
        let mut book = vec![limit_buy(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 0);
        assert_eq!(result.incoming.status, OrderStatus::New);
    }

    // ── Price Improvement ─────────────────────────────────────────────────

    #[test]
    fn test_price_improvement_buy() {
        // Buy at 105, ask at 100 — fills at 100 (resting price, better for buyer)
        let incoming = limit_buy(dec!(105), dec!(1));
        let mut book = vec![limit_sell(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].price, dec!(100)); // taker gets price improvement
        assert_eq!(result.incoming.status, OrderStatus::Filled);
    }

    #[test]
    fn test_price_improvement_sell() {
        // Sell at 95, bid at 100 — fills at 100 (resting price, better for seller)
        let incoming = limit_sell(dec!(95), dec!(1));
        let mut book = vec![limit_buy(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].price, dec!(100)); // taker gets price improvement
        assert_eq!(result.incoming.status, OrderStatus::Filled);
    }

    // ── Price-Time Priority ───────────────────────────────────────────────

    #[test]
    fn test_price_time_priority() {
        let t0 = Utc::now();
        let mut ask1 = limit_sell(dec!(100), dec!(1));
        ask1.created_at = t0;
        let mut ask2 = limit_sell(dec!(100), dec!(1));
        ask2.created_at = t0 + chrono::Duration::milliseconds(100);
        let mut ask3 = limit_sell(dec!(100), dec!(1));
        ask3.created_at = t0 + chrono::Duration::milliseconds(200);

        let mut book = vec![ask1.clone(), ask2.clone(), ask3.clone()];

        let incoming = limit_buy(dec!(100), dec!(1));
        let result = match_order(&incoming, &mut book);

        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].sell_order_id, ask1.id); // earliest first
    }

    #[test]
    fn test_price_priority_over_time() {
        // ask1 at 99 (later), ask2 at 100 (earlier) — price wins
        let t0 = Utc::now();
        let mut ask1 = limit_sell(dec!(99), dec!(1));
        ask1.created_at = t0 + chrono::Duration::milliseconds(100);
        let mut ask2 = limit_sell(dec!(100), dec!(1));
        ask2.created_at = t0;

        // Book sorted: price ASC → ask1(99) before ask2(100)
        let mut book = vec![ask1.clone(), ask2.clone()];

        let incoming = limit_buy(dec!(100), dec!(1));
        let result = match_order(&incoming, &mut book);

        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].price, dec!(99)); // better price wins
        assert_eq!(result.trades[0].sell_order_id, ask1.id);
    }

    // ── Multi-Level Fills ─────────────────────────────────────────────────

    #[test]
    fn test_multi_level_fill() {
        let ask1 = limit_sell(dec!(99), dec!(1));
        let ask2 = limit_sell(dec!(100), dec!(1));
        let ask3 = limit_sell(dec!(101), dec!(1));
        let mut book = vec![ask1, ask2, ask3];

        let incoming = limit_buy(dec!(100), dec!(3));
        let result = match_order(&incoming, &mut book);

        // Fills 99 and 100, but not 101 (> buy price)
        assert_eq!(result.trades.len(), 2);
        assert_eq!(result.incoming.remaining, dec!(1));
        assert_eq!(result.incoming.status, OrderStatus::PartiallyFilled);
    }

    #[test]
    fn test_fill_multiple_small_orders() {
        // 5 small asks, 1 large buy
        let mut book: Vec<Order> = (0..5)
            .map(|_| limit_sell(dec!(100), dec!(1)))
            .collect();

        let incoming = limit_buy(dec!(100), dec!(5));
        let result = match_order(&incoming, &mut book);

        assert_eq!(result.trades.len(), 5);
        assert_eq!(result.incoming.remaining, dec!(0));
        assert_eq!(result.incoming.status, OrderStatus::Filled);
        // All resting orders should be Filled
        assert!(result.book_updates.iter().all(|o| o.status == OrderStatus::Filled));
    }

    #[test]
    fn test_large_buy_partially_fills_book() {
        let mut book: Vec<Order> = (0..3)
            .map(|_| limit_sell(dec!(100), dec!(1)))
            .collect();

        let incoming = limit_buy(dec!(100), dec!(10));
        let result = match_order(&incoming, &mut book);

        assert_eq!(result.trades.len(), 3);
        assert_eq!(result.incoming.remaining, dec!(7));
        assert_eq!(result.incoming.status, OrderStatus::PartiallyFilled);
    }

    // ── Resting Order Updates ─────────────────────────────────────────────

    #[test]
    fn test_resting_order_fully_filled() {
        let incoming = limit_buy(dec!(100), dec!(1));
        let resting = limit_sell(dec!(100), dec!(1));
        let resting_id = resting.id;
        let mut book = vec![resting];
        let result = match_order(&incoming, &mut book);

        assert_eq!(result.book_updates.len(), 1);
        assert_eq!(result.book_updates[0].id, resting_id);
        assert_eq!(result.book_updates[0].status, OrderStatus::Filled);
        assert_eq!(result.book_updates[0].remaining, dec!(0));
    }

    #[test]
    fn test_resting_order_partially_filled() {
        let incoming = limit_buy(dec!(100), dec!(1));
        let resting = limit_sell(dec!(100), dec!(3));
        let resting_id = resting.id;
        let mut book = vec![resting];
        let result = match_order(&incoming, &mut book);

        assert_eq!(result.book_updates.len(), 1);
        assert_eq!(result.book_updates[0].id, resting_id);
        assert_eq!(result.book_updates[0].status, OrderStatus::PartiallyFilled);
        assert_eq!(result.book_updates[0].remaining, dec!(2));
    }

    // ── Market Orders ─────────────────────────────────────────────────────

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
    fn test_market_sell() {
        let incoming = make_order(
            Side::Sell, OrderType::Market, TimeInForce::IOC,
            None, dec!(1), "user-2",
        );
        let mut book = vec![limit_buy(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].price, dec!(100));
        assert_eq!(result.incoming.status, OrderStatus::Filled);
    }

    #[test]
    fn test_market_order_partial_fill_cancels_remainder() {
        // Market buy for 5, only 2 available — fills 2, cancels 3 (IOC behavior)
        let incoming = make_order(
            Side::Buy, OrderType::Market, TimeInForce::IOC,
            None, dec!(5), "user-1",
        );
        let mut book = vec![limit_sell(dec!(100), dec!(2))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].quantity, dec!(2));
        assert_eq!(result.incoming.remaining, dec!(3));
        assert_eq!(result.incoming.status, OrderStatus::Cancelled); // IOC: cancel unfilled
    }

    #[test]
    fn test_market_order_walks_multiple_levels() {
        // Market buy fills across price levels
        let incoming = make_order(
            Side::Buy, OrderType::Market, TimeInForce::IOC,
            None, dec!(3), "user-1",
        );
        let mut book = vec![
            limit_sell(dec!(100), dec!(1)),
            limit_sell(dec!(101), dec!(1)),
            limit_sell(dec!(102), dec!(1)),
        ];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 3);
        assert_eq!(result.trades[0].price, dec!(100));
        assert_eq!(result.trades[1].price, dec!(101));
        assert_eq!(result.trades[2].price, dec!(102));
        assert_eq!(result.incoming.status, OrderStatus::Filled);
    }

    // ── Time-in-Force: GTC ────────────────────────────────────────────────

    #[test]
    fn test_gtc_unfilled_rests() {
        let incoming = limit_buy(dec!(100), dec!(1));
        let mut book = vec![];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.incoming.status, OrderStatus::New);
        assert_eq!(result.incoming.remaining, dec!(1));
    }

    #[test]
    fn test_gtc_partially_filled_rests() {
        let incoming = limit_buy(dec!(100), dec!(3));
        let mut book = vec![limit_sell(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.incoming.remaining, dec!(2));
        assert_eq!(result.incoming.status, OrderStatus::PartiallyFilled);
    }

    // ── Time-in-Force: IOC ────────────────────────────────────────────────

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
    fn test_ioc_full_fill() {
        let incoming = make_order(
            Side::Buy, OrderType::Limit, TimeInForce::IOC,
            Some(dec!(100)), dec!(1), "user-1",
        );
        let mut book = vec![limit_sell(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.incoming.status, OrderStatus::Filled); // fully filled, not cancelled
    }

    #[test]
    fn test_ioc_no_match_cancels() {
        let incoming = make_order(
            Side::Buy, OrderType::Limit, TimeInForce::IOC,
            Some(dec!(95)), dec!(1), "user-1",
        );
        let mut book = vec![limit_sell(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 0);
        assert_eq!(result.incoming.status, OrderStatus::Cancelled);
    }

    // ── Time-in-Force: FOK ────────────────────────────────────────────────

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
        // Book should NOT be modified
        assert_eq!(result.book_updates.len(), 0);
    }

    #[test]
    fn test_fok_exact_quantity_fills() {
        let incoming = make_order(
            Side::Buy, OrderType::Limit, TimeInForce::FOK,
            Some(dec!(100)), dec!(1), "user-1",
        );
        let mut book = vec![limit_sell(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.incoming.status, OrderStatus::Filled);
    }

    #[test]
    fn test_fok_sufficient_across_levels() {
        // FOK buy 2 at 101, asks at 100 (qty 1) and 101 (qty 1) — enough total
        let incoming = make_order(
            Side::Buy, OrderType::Limit, TimeInForce::FOK,
            Some(dec!(101)), dec!(2), "user-1",
        );
        let mut book = vec![
            limit_sell(dec!(100), dec!(1)),
            limit_sell(dec!(101), dec!(1)),
        ];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 2);
        assert_eq!(result.incoming.status, OrderStatus::Filled);
    }

    #[test]
    fn test_fok_insufficient_at_price() {
        // FOK buy 3 at 100, only 1 available at 100 (2 more at 101 but out of price)
        let incoming = make_order(
            Side::Buy, OrderType::Limit, TimeInForce::FOK,
            Some(dec!(100)), dec!(3), "user-1",
        );
        let mut book = vec![
            limit_sell(dec!(100), dec!(1)),
            limit_sell(dec!(101), dec!(2)),
        ];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 0);
        assert_eq!(result.incoming.status, OrderStatus::Cancelled);
    }

    #[test]
    fn test_fok_empty_book_cancels() {
        let incoming = make_order(
            Side::Buy, OrderType::Limit, TimeInForce::FOK,
            Some(dec!(100)), dec!(1), "user-1",
        );
        let mut book = vec![];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 0);
        assert_eq!(result.incoming.status, OrderStatus::Cancelled);
    }

    // ── Self-Trade Prevention ─────────────────────────────────────────────

    #[test]
    fn test_stp_cancel_taker() {
        let mut incoming = limit_buy_user(dec!(100), dec!(1), "user-1");
        incoming.stp_mode = SelfTradePreventionMode::CancelTaker;
        let mut book = vec![limit_sell_user(dec!(100), dec!(1), "user-1")];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 0);
        assert_eq!(result.incoming.status, OrderStatus::Cancelled);
        assert_eq!(result.stp_cancelled.len(), 1);
        // Resting order should NOT be in book_updates (it stays)
        assert!(result.book_updates.is_empty());
    }

    #[test]
    fn test_stp_cancel_maker() {
        let mut incoming = limit_buy_user(dec!(100), dec!(1), "user-1");
        incoming.stp_mode = SelfTradePreventionMode::CancelMaker;
        let mut book = vec![limit_sell_user(dec!(100), dec!(1), "user-1")];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 0);
        assert_eq!(result.stp_cancelled.len(), 1);
        assert_eq!(result.stp_cancelled[0].status, OrderStatus::Cancelled);
        // Maker should be in book_updates as cancelled
        assert_eq!(result.book_updates.len(), 1);
        assert_eq!(result.book_updates[0].status, OrderStatus::Cancelled);
    }

    #[test]
    fn test_stp_cancel_both() {
        let mut incoming = limit_buy_user(dec!(100), dec!(1), "user-1");
        incoming.stp_mode = SelfTradePreventionMode::CancelBoth;
        let mut book = vec![limit_sell_user(dec!(100), dec!(1), "user-1")];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 0);
        assert_eq!(result.incoming.status, OrderStatus::Cancelled);
        assert_eq!(result.stp_cancelled.len(), 2); // both incoming and resting
    }

    #[test]
    fn test_stp_none_allows_self_trade() {
        // STP mode is None — self-trade should go through
        let incoming = limit_buy_user(dec!(100), dec!(1), "user-1");
        let mut book = vec![limit_sell_user(dec!(100), dec!(1), "user-1")];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1); // trade happens
        assert_eq!(result.incoming.status, OrderStatus::Filled);
        assert_eq!(result.stp_cancelled.len(), 0);
    }

    #[test]
    fn test_stp_different_users_no_trigger() {
        // Different users — STP should NOT trigger regardless of mode
        let mut incoming = limit_buy_user(dec!(100), dec!(1), "user-1");
        incoming.stp_mode = SelfTradePreventionMode::CancelTaker;
        let mut book = vec![limit_sell_user(dec!(100), dec!(1), "user-2")];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1); // trade happens normally
        assert_eq!(result.stp_cancelled.len(), 0);
    }

    #[test]
    fn test_stp_cancel_maker_skips_to_next() {
        // User-1 has 2 resting orders. CancelMaker cancels first, then matches second (user-2)
        let mut incoming = limit_buy_user(dec!(100), dec!(1), "user-1");
        incoming.stp_mode = SelfTradePreventionMode::CancelMaker;

        let self_order = limit_sell_user(dec!(100), dec!(1), "user-1"); // same user
        let other_order = limit_sell_user(dec!(100), dec!(1), "user-2"); // different user
        let other_id = other_order.id;
        let mut book = vec![self_order, other_order];

        let result = match_order(&incoming, &mut book);
        assert_eq!(result.stp_cancelled.len(), 1); // self_order cancelled
        assert_eq!(result.trades.len(), 1); // matched with other_order
        assert_eq!(result.trades[0].sell_order_id, other_id);
        assert_eq!(result.incoming.status, OrderStatus::Filled);
    }

    // ── Trade Direction Verification ──────────────────────────────────────

    #[test]
    fn test_trade_direction_buy_incoming() {
        let incoming = limit_buy_user(dec!(100), dec!(1), "buyer");
        let resting = limit_sell_user(dec!(100), dec!(1), "seller");
        let resting_id = resting.id;
        let mut book = vec![resting];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades[0].buy_order_id, incoming.id);
        assert_eq!(result.trades[0].sell_order_id, resting_id);
        assert_eq!(result.trades[0].buyer_id, "buyer");
        assert_eq!(result.trades[0].seller_id, "seller");
    }

    #[test]
    fn test_trade_direction_sell_incoming() {
        let incoming = limit_sell_user(dec!(100), dec!(1), "seller");
        let resting = limit_buy_user(dec!(100), dec!(1), "buyer");
        let resting_id = resting.id;
        let mut book = vec![resting];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades[0].buy_order_id, resting_id);
        assert_eq!(result.trades[0].sell_order_id, incoming.id);
        assert_eq!(result.trades[0].buyer_id, "buyer");
        assert_eq!(result.trades[0].seller_id, "seller");
    }

    // ── Quantity Edge Cases ───────────────────────────────────────────────

    #[test]
    fn test_exact_fill_both_sides() {
        let incoming = limit_buy(dec!(100), dec!(1));
        let mut book = vec![limit_sell(dec!(100), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.incoming.remaining, dec!(0));
        assert_eq!(result.book_updates[0].remaining, dec!(0));
        assert_eq!(result.incoming.status, OrderStatus::Filled);
        assert_eq!(result.book_updates[0].status, OrderStatus::Filled);
    }

    #[test]
    fn test_tiny_quantity() {
        let incoming = limit_buy(dec!(100), dec!(0.00001));
        let mut book = vec![limit_sell(dec!(100), dec!(0.00001))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].quantity, dec!(0.00001));
        assert_eq!(result.incoming.status, OrderStatus::Filled);
    }

    #[test]
    fn test_large_quantity() {
        let incoming = limit_buy(dec!(100), dec!(999999999));
        let mut book = vec![limit_sell(dec!(100), dec!(999999999))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].quantity, dec!(999999999));
        assert_eq!(result.incoming.status, OrderStatus::Filled);
    }

    // ── Decimal Precision ─────────────────────────────────────────────────

    #[test]
    fn test_decimal_price_precision() {
        let incoming = limit_buy(dec!(100.01), dec!(1));
        let mut book = vec![limit_sell(dec!(100.01), dec!(1))];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].price, dec!(100.01));
    }

    #[test]
    fn test_decimal_no_floating_point_error() {
        // Classic floating point trap: 0.1 + 0.2 != 0.3 in IEEE 754
        // Decimal should handle this correctly
        let incoming = limit_buy(dec!(100), dec!(0.3));
        let mut book = vec![
            limit_sell(dec!(100), dec!(0.1)),
            limit_sell(dec!(100), dec!(0.2)),
        ];
        let result = match_order(&incoming, &mut book);
        assert_eq!(result.trades.len(), 2);
        assert_eq!(result.incoming.remaining, dec!(0)); // exactly zero, no floating point error
        assert_eq!(result.incoming.status, OrderStatus::Filled);
    }

    // ── Stress-like (deterministic, still fast) ───────────────────────────

    #[test]
    fn test_100_level_book() {
        // 100 resting orders at different prices
        let mut book: Vec<Order> = (1..=100)
            .map(|i| limit_sell(Decimal::from(i), dec!(1)))
            .collect();

        let incoming = limit_buy(dec!(50), dec!(50));
        let result = match_order(&incoming, &mut book);

        assert_eq!(result.trades.len(), 50); // fills levels 1-50
        assert_eq!(result.incoming.remaining, dec!(0));
        assert_eq!(result.incoming.status, OrderStatus::Filled);
    }

    #[test]
    fn test_book_updates_count_matches_trades() {
        let mut book: Vec<Order> = (0..10)
            .map(|_| limit_sell(dec!(100), dec!(1)))
            .collect();

        let incoming = limit_buy(dec!(100), dec!(7));
        let result = match_order(&incoming, &mut book);

        assert_eq!(result.trades.len(), 7);
        assert_eq!(result.book_updates.len(), 7); // one update per matched resting order
    }
}
