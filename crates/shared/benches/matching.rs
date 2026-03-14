//! Criterion benchmarks for the matching engine.

use chrono::Utc;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::str::FromStr;
use uuid::Uuid;

use sme_shared::{
    Order, OrderStatus, OrderType, SelfTradePreventionMode, Side, TimeInForce,
    engine::match_order,
};

fn make_order(
    side: Side,
    price: Option<Decimal>,
    qty: Decimal,
    order_type: OrderType,
) -> Order {
    Order {
        id: Uuid::new_v4(),
        user_id: "bench-user".to_string(),
        pair_id: "BTC-USDT".to_string(),
        side,
        order_type,
        tif: TimeInForce::GTC,
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

fn make_ask(price: &str, qty: &str) -> Order {
    make_order(
        Side::Sell,
        Some(Decimal::from_str(price).unwrap()),
        Decimal::from_str(qty).unwrap(),
        OrderType::Limit,
    )
}

fn make_bid(price: &str, qty: &str) -> Order {
    make_order(
        Side::Buy,
        Some(Decimal::from_str(price).unwrap()),
        Decimal::from_str(qty).unwrap(),
        OrderType::Limit,
    )
}

// ── Benchmark 1: Single limit match (1v1) ────────────────────────────────────

fn bench_single_match(c: &mut Criterion) {
    c.bench_function("single_limit_match_1v1", |b| {
        b.iter(|| {
            let incoming = make_bid("50000", "1");
            let mut book = vec![make_ask("49999", "1")];
            black_box(match_order(&incoming, &mut book));
        });
    });
}

// ── Benchmark 2: Market order walking 100-level book ─────────────────────────

fn bench_market_walk_100(c: &mut Criterion) {
    // Pre-build a 100-level ask book
    let book_100: Vec<Order> = (1..=100u32)
        .map(|i| make_ask(&format!("{}", 49000 + i), "0.01"))
        .collect();

    c.bench_function("market_order_walk_100_levels", |b| {
        b.iter(|| {
            let incoming = make_order(Side::Buy, None, dec!(1.0), OrderType::Market);
            let mut book = book_100.clone();
            black_box(match_order(&incoming, &mut book));
        });
    });
}

// ── Benchmark 3: 1000 sequential matches same pair ───────────────────────────

fn bench_1000_sequential(c: &mut Criterion) {
    c.bench_function("1000_sequential_matches", |b| {
        b.iter(|| {
            let mut matched = 0usize;
            for i in 0..1000u32 {
                let incoming = make_bid("50000", "0.001");
                let mut book = vec![make_ask(&format!("{}", 49900 + i % 100), "0.001")];
                let result = match_order(&incoming, &mut book);
                matched += result.trades.len();
            }
            black_box(matched);
        });
    });
}

// ── Benchmark 4: 10 pairs × 100 orders each ──────────────────────────────────

fn bench_multi_pair_10x100(c: &mut Criterion) {
    c.bench_function("multi_pair_10x100", |b| {
        b.iter(|| {
            let mut total_trades = 0usize;
            for _pair in 0..10 {
                for i in 0..100u32 {
                    let incoming = make_bid("50000", "0.001");
                    let mut book = vec![make_ask(&format!("{}", 49900 + i % 50), "0.001")];
                    let result = match_order(&incoming, &mut book);
                    total_trades += result.trades.len();
                }
            }
            black_box(total_trades);
        });
    });
}

// ── Benchmark 5: 100 pairs × 10 orders each ──────────────────────────────────

fn bench_multi_pair_100x10(c: &mut Criterion) {
    c.bench_function("multi_pair_100x10", |b| {
        b.iter(|| {
            let mut total_trades = 0usize;
            for _pair in 0..100 {
                for i in 0..10u32 {
                    let incoming = make_bid("50000", "0.001");
                    let mut book = vec![make_ask(&format!("{}", 49900 + i % 5), "0.001")];
                    let result = match_order(&incoming, &mut book);
                    total_trades += result.trades.len();
                }
            }
            black_box(total_trades);
        });
    });
}

criterion_group!(
    benches,
    bench_single_match,
    bench_market_walk_100,
    bench_1000_sequential,
    bench_multi_pair_10x100,
    bench_multi_pair_100x10,
);
criterion_main!(benches);
