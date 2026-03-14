use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Order Types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(&self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderType {
    Limit,
    Market,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeInForce {
    /// Good Til Canceled — rests until filled or canceled
    GTC,
    /// Immediate or Cancel — fill what you can, cancel rest
    IOC,
    /// Fill or Kill — fill entirely or cancel
    FOK,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    New,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SelfTradePreventionMode {
    /// No STP — allow self-trades
    None,
    /// Cancel the incoming (taker) order
    CancelTaker,
    /// Cancel the resting (maker) order
    CancelMaker,
    /// Cancel both orders
    CancelBoth,
}

// ── Core Structs ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: Uuid,
    pub user_id: String,
    pub pair_id: String,
    pub side: Side,
    pub order_type: OrderType,
    pub tif: TimeInForce,
    pub price: Option<Decimal>,    // None for market orders
    pub quantity: Decimal,
    pub remaining: Decimal,
    pub status: OrderStatus,
    pub stp_mode: SelfTradePreventionMode,
    pub version: i64,
    pub sequence: i64,             // per-pair sequence number
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Client-provided idempotency key. If set, duplicate submissions
    /// with the same (user_id, client_order_id) return the existing order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub id: Uuid,
    pub pair_id: String,
    pub buy_order_id: Uuid,
    pub sell_order_id: Uuid,
    pub buyer_id: String,
    pub seller_id: String,
    pub price: Decimal,
    pub quantity: Decimal,
    pub sequence: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pair {
    pub id: String,
    pub base: String,
    pub quote: String,
    pub tick_size: Decimal,        // minimum price increment
    pub lot_size: Decimal,         // minimum quantity increment
    pub min_order_size: Decimal,
    pub max_order_size: Decimal,
    pub price_precision: u8,       // decimal places for price
    pub qty_precision: u8,         // decimal places for quantity
    pub price_band_pct: Decimal,   // max % deviation from last trade
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Balance {
    pub user_id: String,
    pub asset: String,
    pub available: Decimal,
    pub locked: Decimal,           // in open orders
}

// ── Audit Events ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuditEvent {
    OrderCreated { order: Order },
    OrderModified { order_id: Uuid, old_price: Option<Decimal>, new_price: Option<Decimal>, old_qty: Decimal, new_qty: Decimal },
    OrderCancelled { order_id: Uuid, reason: String },
    OrderRejected { order_id: Uuid, reason: String },
    TradeExecuted { trade: Trade },
    SelfTradePrevented { taker_id: Uuid, maker_id: Uuid, mode: SelfTradePreventionMode },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    pub sequence: i64,
    pub pair_id: String,
    pub timestamp: DateTime<Utc>,
    pub event: AuditEvent,
}
