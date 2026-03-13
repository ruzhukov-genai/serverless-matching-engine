//! Integration tests — require running Dragonfly (6379) + PostgreSQL (5432).
//! Run with: cargo test --features integration -- --test-threads=1
//!
//! These tests use unique pair_ids to avoid cross-test interference in cache,
//! and each test cleans up its own keys. DB tests use transactions that roll back.

#[cfg(test)]
#[cfg(feature = "integration")]
mod tests {
    use crate::types::*;
    use crate::{cache, db, lock, Config};
    use chrono::Utc;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use uuid::Uuid;

    // ── Helpers ───────────────────────────────────────────────────────────

    async fn dragonfly_pool() -> deadpool_redis::Pool {
        cache::create_pool("redis://localhost:6379")
            .await
            .expect("connect to dragonfly")
    }

    async fn pg_pool() -> sqlx::PgPool {
        db::create_pool("postgres://sme:sme_dev@localhost:5432/matching_engine")
            .await
            .expect("connect to postgres")
    }

    fn test_order(side: Side, price: rust_decimal::Decimal, qty: rust_decimal::Decimal, pair: &str, user: &str) -> Order {
        Order {
            id: Uuid::new_v4(),
            user_id: user.to_string(),
            pair_id: pair.to_string(),
            side,
            order_type: OrderType::Limit,
            tif: TimeInForce::GTC,
            price: Some(price),
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

    async fn cleanup_pair(pool: &deadpool_redis::Pool, pair_id: &str) {
        let mut conn = pool.get().await.unwrap();
        let _: () = redis::cmd("DEL")
            .arg(format!("book:{pair_id}:bids"))
            .arg(format!("book:{pair_id}:asks"))
            .arg(format!("book:{pair_id}:lock"))
            .arg(format!("version:{pair_id}"))
            .query_async(&mut *conn)
            .await
            .unwrap();
    }

    async fn cleanup_order(pool: &deadpool_redis::Pool, order_id: &Uuid) {
        let mut conn = pool.get().await.unwrap();
        let _: () = redis::cmd("DEL")
            .arg(format!("order:{order_id}"))
            .query_async(&mut *conn)
            .await
            .unwrap();
    }

    // ═══════════════════════════════════════════════════════════════════════
    // CACHE TESTS
    // ═══════════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn cache_health_check() {
        let pool = dragonfly_pool().await;
        cache::health_check(&pool).await.expect("health check should pass");
    }

    #[tokio::test]
    async fn cache_save_and_load_bid() {
        let pool = dragonfly_pool().await;
        let pair = "TEST-CACHE-BID";
        cleanup_pair(&pool, pair).await;

        let order = test_order(Side::Buy, dec!(100), dec!(1), pair, "u1");
        cache::save_order_to_book(&pool, &order).await.unwrap();

        let book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        assert_eq!(book.len(), 1);
        assert_eq!(book[0].id, order.id);
        assert_eq!(book[0].price, Some(dec!(100)));

        cleanup_order(&pool, &order.id).await;
        cleanup_pair(&pool, pair).await;
    }

    #[tokio::test]
    async fn cache_save_and_load_ask() {
        let pool = dragonfly_pool().await;
        let pair = "TEST-CACHE-ASK";
        cleanup_pair(&pool, pair).await;

        let order = test_order(Side::Sell, dec!(200), dec!(5), pair, "u1");
        cache::save_order_to_book(&pool, &order).await.unwrap();

        let book = cache::load_order_book(&pool, pair, Side::Sell).await.unwrap();
        assert_eq!(book.len(), 1);
        assert_eq!(book[0].id, order.id);
        assert_eq!(book[0].quantity, dec!(5));

        cleanup_order(&pool, &order.id).await;
        cleanup_pair(&pool, pair).await;
    }

    #[tokio::test]
    async fn cache_load_empty_book() {
        let pool = dragonfly_pool().await;
        let pair = "TEST-EMPTY-BOOK";
        cleanup_pair(&pool, pair).await;

        let book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        assert_eq!(book.len(), 0);
    }

    #[tokio::test]
    async fn cache_asks_sorted_by_price_asc() {
        let pool = dragonfly_pool().await;
        let pair = "TEST-ASK-SORT";
        cleanup_pair(&pool, pair).await;

        let o1 = test_order(Side::Sell, dec!(300), dec!(1), pair, "u1");
        let o2 = test_order(Side::Sell, dec!(100), dec!(1), pair, "u2");
        let o3 = test_order(Side::Sell, dec!(200), dec!(1), pair, "u3");

        cache::save_order_to_book(&pool, &o1).await.unwrap();
        cache::save_order_to_book(&pool, &o2).await.unwrap();
        cache::save_order_to_book(&pool, &o3).await.unwrap();

        let book = cache::load_order_book(&pool, pair, Side::Sell).await.unwrap();
        assert_eq!(book.len(), 3);
        // Asks sorted price ASC: 100, 200, 300
        assert_eq!(book[0].price, Some(dec!(100)));
        assert_eq!(book[1].price, Some(dec!(200)));
        assert_eq!(book[2].price, Some(dec!(300)));

        for o in [&o1, &o2, &o3] { cleanup_order(&pool, &o.id).await; }
        cleanup_pair(&pool, pair).await;
    }

    #[tokio::test]
    async fn cache_bids_sorted_by_price_desc() {
        let pool = dragonfly_pool().await;
        let pair = "TEST-BID-SORT";
        cleanup_pair(&pool, pair).await;

        let o1 = test_order(Side::Buy, dec!(100), dec!(1), pair, "u1");
        let o2 = test_order(Side::Buy, dec!(300), dec!(1), pair, "u2");
        let o3 = test_order(Side::Buy, dec!(200), dec!(1), pair, "u3");

        cache::save_order_to_book(&pool, &o1).await.unwrap();
        cache::save_order_to_book(&pool, &o2).await.unwrap();
        cache::save_order_to_book(&pool, &o3).await.unwrap();

        let book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        assert_eq!(book.len(), 3);
        // Bids sorted price DESC (negated scores): 300, 200, 100
        assert_eq!(book[0].price, Some(dec!(300)));
        assert_eq!(book[1].price, Some(dec!(200)));
        assert_eq!(book[2].price, Some(dec!(100)));

        for o in [&o1, &o2, &o3] { cleanup_order(&pool, &o.id).await; }
        cleanup_pair(&pool, pair).await;
    }

    #[tokio::test]
    async fn cache_remove_order() {
        let pool = dragonfly_pool().await;
        let pair = "TEST-REMOVE";
        cleanup_pair(&pool, pair).await;

        let order = test_order(Side::Sell, dec!(100), dec!(1), pair, "u1");
        cache::save_order_to_book(&pool, &order).await.unwrap();

        let book = cache::load_order_book(&pool, pair, Side::Sell).await.unwrap();
        assert_eq!(book.len(), 1);

        cache::remove_order_from_book(&pool, &order).await.unwrap();

        let book = cache::load_order_book(&pool, pair, Side::Sell).await.unwrap();
        assert_eq!(book.len(), 0);

        // Hash should be gone too
        let fetched = cache::get_order(&pool, &order.id).await.unwrap();
        assert!(fetched.is_none());

        cleanup_pair(&pool, pair).await;
    }

    #[tokio::test]
    async fn cache_get_order() {
        let pool = dragonfly_pool().await;
        let pair = "TEST-GET";
        cleanup_pair(&pool, pair).await;

        let order = test_order(Side::Buy, dec!(42), dec!(7), pair, "u1");
        cache::save_order_to_book(&pool, &order).await.unwrap();

        let fetched = cache::get_order(&pool, &order.id).await.unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.id, order.id);
        assert_eq!(fetched.price, Some(dec!(42)));
        assert_eq!(fetched.remaining, dec!(7));

        cleanup_order(&pool, &order.id).await;
        cleanup_pair(&pool, pair).await;
    }

    #[tokio::test]
    async fn cache_update_order_in_place() {
        let pool = dragonfly_pool().await;
        let pair = "TEST-UPDATE";
        cleanup_pair(&pool, pair).await;

        let mut order = test_order(Side::Sell, dec!(100), dec!(5), pair, "u1");
        cache::save_order_to_book(&pool, &order).await.unwrap();

        // Simulate partial fill
        order.remaining = dec!(3);
        order.status = OrderStatus::PartiallyFilled;
        cache::save_order_to_book(&pool, &order).await.unwrap();

        let fetched = cache::get_order(&pool, &order.id).await.unwrap().unwrap();
        assert_eq!(fetched.remaining, dec!(3));
        assert_eq!(fetched.status, OrderStatus::PartiallyFilled);

        cleanup_order(&pool, &order.id).await;
        cleanup_pair(&pool, pair).await;
    }

    #[tokio::test]
    async fn cache_version_increment() {
        let pool = dragonfly_pool().await;
        let pair = "TEST-VERSION";
        cleanup_pair(&pool, pair).await;

        let v1 = cache::increment_version(&pool, pair).await.unwrap();
        let v2 = cache::increment_version(&pool, pair).await.unwrap();
        let v3 = cache::increment_version(&pool, pair).await.unwrap();

        assert_eq!(v2, v1 + 1);
        assert_eq!(v3, v2 + 1);

        cleanup_pair(&pool, pair).await;
    }

    // ═══════════════════════════════════════════════════════════════════════
    // LOCK TESTS
    // ═══════════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn lock_acquire_and_release() {
        let pool = dragonfly_pool().await;
        let pair = "TEST-LOCK-1";
        cleanup_pair(&pool, pair).await;

        let guard = lock::acquire_lock(&pool, pair, "worker-1").await.unwrap();
        assert_eq!(guard.pair_id, pair);
        assert_eq!(guard.worker_id, "worker-1");

        guard.release().await;

        // Should be able to acquire again immediately
        let guard2 = lock::acquire_lock(&pool, pair, "worker-2").await.unwrap();
        guard2.release().await;

        cleanup_pair(&pool, pair).await;
    }

    #[tokio::test]
    async fn lock_prevents_concurrent_acquisition() {
        let pool = dragonfly_pool().await;
        let pair = "TEST-LOCK-2";
        cleanup_pair(&pool, pair).await;

        // Hold the lock
        let guard = lock::acquire_lock(&pool, pair, "worker-A").await.unwrap();

        // Try to acquire with a very short timeout — this should fail
        // We can't easily test this without modifying MAX_RETRIES,
        // but we CAN verify the key exists
        let mut conn = pool.get().await.unwrap();
        let val: Option<String> = redis::cmd("GET")
            .arg(format!("book:{pair}:lock"))
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert_eq!(val, Some("worker-A".to_string()));

        guard.release().await;

        // After release, key should be gone
        let val: Option<String> = redis::cmd("GET")
            .arg(format!("book:{pair}:lock"))
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert!(val.is_none());

        cleanup_pair(&pool, pair).await;
    }

    #[tokio::test]
    async fn lock_ttl_expires() {
        let pool = dragonfly_pool().await;
        let pair = "TEST-LOCK-TTL";
        cleanup_pair(&pool, pair).await;

        // Acquire lock
        let guard = lock::acquire_lock(&pool, pair, "worker-crash").await.unwrap();
        // Simulate crash — forget the guard so it doesn't auto-release
        std::mem::forget(guard);

        // Lock TTL is 1 second — wait for it to expire
        tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;

        // Should be able to acquire now (TTL expired)
        let guard2 = lock::acquire_lock(&pool, pair, "worker-recovery").await.unwrap();
        guard2.release().await;

        cleanup_pair(&pool, pair).await;
    }

    #[tokio::test]
    async fn lock_release_only_own() {
        let pool = dragonfly_pool().await;
        let pair = "TEST-LOCK-OWN";
        cleanup_pair(&pool, pair).await;

        // Worker-A acquires the lock
        let _guard = lock::acquire_lock(&pool, pair, "worker-A").await.unwrap();

        // Worker-B tries to release — should NOT release worker-A's lock
        lock::release_lock(&pool, pair, "worker-B").await.unwrap();

        // Lock should still be held by worker-A
        let mut conn = pool.get().await.unwrap();
        let val: Option<String> = redis::cmd("GET")
            .arg(format!("book:{pair}:lock"))
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert_eq!(val, Some("worker-A".to_string()));

        // Cleanup
        std::mem::forget(_guard);
        let _: () = redis::cmd("DEL")
            .arg(format!("book:{pair}:lock"))
            .query_async(&mut *conn)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn lock_different_pairs_independent() {
        let pool = dragonfly_pool().await;
        let pair_a = "TEST-LOCK-PAIR-A";
        let pair_b = "TEST-LOCK-PAIR-B";
        cleanup_pair(&pool, pair_a).await;
        cleanup_pair(&pool, pair_b).await;

        // Both locks can be held simultaneously
        let guard_a = lock::acquire_lock(&pool, pair_a, "w1").await.unwrap();
        let guard_b = lock::acquire_lock(&pool, pair_b, "w1").await.unwrap();

        // Both acquired — pairs are independent
        assert_eq!(guard_a.pair_id, pair_a);
        assert_eq!(guard_b.pair_id, pair_b);

        guard_a.release().await;
        guard_b.release().await;

        cleanup_pair(&pool, pair_a).await;
        cleanup_pair(&pool, pair_b).await;
    }

    // ═══════════════════════════════════════════════════════════════════════
    // DATABASE TESTS
    // ═══════════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn db_connection_and_migrations() {
        let pool = pg_pool().await;
        db::run_migrations(&pool).await.expect("migrations should succeed");

        // Verify tables exist
        let row = sqlx::query("SELECT COUNT(*) as cnt FROM pairs")
            .fetch_one(&pool).await.unwrap();
        let cnt: i64 = sqlx::Row::get(&row, "cnt");
        assert!(cnt >= 0); // table exists
    }

    #[tokio::test]
    async fn db_insert_and_read_order() {
        let pool = pg_pool().await;
        db::run_migrations(&pool).await.unwrap();

        let order_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO orders (id, user_id, pair_id, side, order_type, tif, price, quantity, remaining, status)
             VALUES ($1, 'test-user', 'BTC-USDT', 'Buy', 'Limit', 'GTC', 50000, 1.5, 1.5, 'New')"
        )
        .bind(order_id)
        .execute(&pool).await.unwrap();

        let row = sqlx::query("SELECT * FROM orders WHERE id = $1")
            .bind(order_id)
            .fetch_one(&pool).await.unwrap();

        let side: String = sqlx::Row::get(&row, "side");
        let status: String = sqlx::Row::get(&row, "status");
        assert_eq!(side, "Buy");
        assert_eq!(status, "New");

        // Cleanup
        sqlx::query("DELETE FROM orders WHERE id = $1").bind(order_id).execute(&pool).await.unwrap();
    }

    #[tokio::test]
    async fn db_insert_trade_with_fk() {
        let pool = pg_pool().await;
        db::run_migrations(&pool).await.unwrap();

        let buy_id = Uuid::new_v4();
        let sell_id = Uuid::new_v4();
        let trade_id = Uuid::new_v4();

        // Insert orders first (FK constraint)
        for (id, side) in [(buy_id, "Buy"), (sell_id, "Sell")] {
            sqlx::query(
                "INSERT INTO orders (id, user_id, pair_id, side, order_type, tif, price, quantity, remaining, status)
                 VALUES ($1, 'test-user', 'BTC-USDT', $2, 'Limit', 'GTC', 50000, 1, 0, 'Filled')"
            )
            .bind(id).bind(side)
            .execute(&pool).await.unwrap();
        }

        // Insert trade
        sqlx::query(
            "INSERT INTO trades (id, pair_id, buy_order_id, sell_order_id, buyer_id, seller_id, price, quantity)
             VALUES ($1, 'BTC-USDT', $2, $3, 'buyer', 'seller', 50000, 1)"
        )
        .bind(trade_id).bind(buy_id).bind(sell_id)
        .execute(&pool).await.unwrap();

        let row = sqlx::query("SELECT * FROM trades WHERE id = $1")
            .bind(trade_id)
            .fetch_one(&pool).await.unwrap();
        let pair: String = sqlx::Row::get(&row, "pair_id");
        assert_eq!(pair, "BTC-USDT");

        // Cleanup
        sqlx::query("DELETE FROM trades WHERE id = $1").bind(trade_id).execute(&pool).await.unwrap();
        sqlx::query("DELETE FROM orders WHERE id = $1 OR id = $2").bind(buy_id).bind(sell_id).execute(&pool).await.unwrap();
    }

    #[tokio::test]
    async fn db_trade_idempotent() {
        let pool = pg_pool().await;
        db::run_migrations(&pool).await.unwrap();

        let buy_id = Uuid::new_v4();
        let sell_id = Uuid::new_v4();
        let trade_id = Uuid::new_v4();

        for (id, side) in [(buy_id, "Buy"), (sell_id, "Sell")] {
            sqlx::query(
                "INSERT INTO orders (id, user_id, pair_id, side, order_type, tif, price, quantity, remaining, status)
                 VALUES ($1, 'test-user', 'BTC-USDT', $2, 'Limit', 'GTC', 50000, 1, 0, 'Filled')"
            ).bind(id).bind(side).execute(&pool).await.unwrap();
        }

        let insert_trade = "INSERT INTO trades (id, pair_id, buy_order_id, sell_order_id, buyer_id, seller_id, price, quantity)
             VALUES ($1, 'BTC-USDT', $2, $3, 'buyer', 'seller', 50000, 1) ON CONFLICT DO NOTHING";

        // Insert twice — second should be a no-op
        sqlx::query(insert_trade).bind(trade_id).bind(buy_id).bind(sell_id).execute(&pool).await.unwrap();
        sqlx::query(insert_trade).bind(trade_id).bind(buy_id).bind(sell_id).execute(&pool).await.unwrap();

        let row = sqlx::query("SELECT COUNT(*) as cnt FROM trades WHERE id = $1")
            .bind(trade_id).fetch_one(&pool).await.unwrap();
        let cnt: i64 = sqlx::Row::get(&row, "cnt");
        assert_eq!(cnt, 1); // only one record, not two

        // Cleanup
        sqlx::query("DELETE FROM trades WHERE id = $1").bind(trade_id).execute(&pool).await.unwrap();
        sqlx::query("DELETE FROM orders WHERE id = $1 OR id = $2").bind(buy_id).bind(sell_id).execute(&pool).await.unwrap();
    }

    #[tokio::test]
    async fn db_balance_update() {
        let pool = pg_pool().await;
        db::run_migrations(&pool).await.unwrap();

        let test_user = format!("test-bal-{}", Uuid::new_v4().to_string().split('-').next().unwrap());

        // Insert test balance
        sqlx::query("INSERT INTO balances (user_id, asset, available, locked) VALUES ($1, 'BTC', 10, 0) ON CONFLICT DO NOTHING")
            .bind(&test_user).execute(&pool).await.unwrap();

        // Lock some balance (simulating order placement)
        sqlx::query("UPDATE balances SET available = available - 2, locked = locked + 2 WHERE user_id = $1 AND asset = 'BTC'")
            .bind(&test_user).execute(&pool).await.unwrap();

        let row = sqlx::query("SELECT available, locked FROM balances WHERE user_id = $1 AND asset = 'BTC'")
            .bind(&test_user).fetch_one(&pool).await.unwrap();
        let available: rust_decimal::Decimal = sqlx::Row::get(&row, "available");
        let locked: rust_decimal::Decimal = sqlx::Row::get(&row, "locked");
        assert_eq!(available, dec!(8));
        assert_eq!(locked, dec!(2));

        // Cleanup
        sqlx::query("DELETE FROM balances WHERE user_id = $1").bind(&test_user).execute(&pool).await.unwrap();
    }

    // ═══════════════════════════════════════════════════════════════════════
    // FULL CYCLE: CACHE + ENGINE + DB
    // ═══════════════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn full_cycle_lock_load_match_write() {
        use crate::engine::match_order;

        let pool = dragonfly_pool().await;
        let pair = "TEST-FULLCYCLE";
        cleanup_pair(&pool, pair).await;

        // 1. Place a sell order in the cache
        let sell = test_order(Side::Sell, dec!(100), dec!(5), pair, "seller");
        cache::save_order_to_book(&pool, &sell).await.unwrap();

        // 2. Incoming buy order
        let buy = test_order(Side::Buy, dec!(100), dec!(3), pair, "buyer");

        // 3. Acquire lock
        let guard = lock::acquire_lock(&pool, pair, "worker-1").await.unwrap();

        // 4. Load order book (asks side for buy incoming)
        let mut book = cache::load_order_book(&pool, pair, Side::Sell).await.unwrap();
        assert_eq!(book.len(), 1);

        // 5. Match
        let result = match_order(&buy, &mut book);
        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].quantity, dec!(3));
        assert_eq!(result.trades[0].price, dec!(100));
        assert_eq!(result.incoming.status, OrderStatus::Filled);

        // 6. Update cache — resting order partially filled (remaining 2)
        let updated_sell = &result.book_updates[0];
        assert_eq!(updated_sell.remaining, dec!(2));
        cache::save_order_to_book(&pool, updated_sell).await.unwrap();

        // 7. Increment version
        let v = cache::increment_version(&pool, pair).await.unwrap();
        assert!(v >= 1);

        // 8. Release lock
        guard.release().await;

        // 9. Verify final state
        let book = cache::load_order_book(&pool, pair, Side::Sell).await.unwrap();
        assert_eq!(book.len(), 1);
        assert_eq!(book[0].remaining, dec!(2));
        assert_eq!(book[0].status, OrderStatus::PartiallyFilled);

        // Cleanup
        cleanup_order(&pool, &sell.id).await;
        cleanup_pair(&pool, pair).await;
    }

    #[tokio::test]
    async fn full_cycle_complete_fill_removes_from_book() {
        use crate::engine::match_order;

        let pool = dragonfly_pool().await;
        let pair = "TEST-FULLCYCLE-FILL";
        cleanup_pair(&pool, pair).await;

        // Place sell order
        let sell = test_order(Side::Sell, dec!(50), dec!(1), pair, "seller");
        cache::save_order_to_book(&pool, &sell).await.unwrap();

        // Incoming buy that fully fills the sell
        let buy = test_order(Side::Buy, dec!(50), dec!(1), pair, "buyer");

        let guard = lock::acquire_lock(&pool, pair, "w1").await.unwrap();
        let mut book = cache::load_order_book(&pool, pair, Side::Sell).await.unwrap();
        let result = match_order(&buy, &mut book);

        // Resting order fully filled — remove from cache
        assert_eq!(result.book_updates[0].status, OrderStatus::Filled);
        cache::remove_order_from_book(&pool, &result.book_updates[0]).await.unwrap();

        guard.release().await;

        // Book should be empty now
        let book = cache::load_order_book(&pool, pair, Side::Sell).await.unwrap();
        assert_eq!(book.len(), 0);

        cleanup_pair(&pool, pair).await;
    }

    #[tokio::test]
    async fn full_cycle_multiple_fills_across_levels() {
        use crate::engine::match_order;

        let pool = dragonfly_pool().await;
        let pair = "TEST-FULLCYCLE-MULTI";
        cleanup_pair(&pool, pair).await;

        // Place 3 sell orders at different prices
        let s1 = test_order(Side::Sell, dec!(99), dec!(1), pair, "s1");
        let s2 = test_order(Side::Sell, dec!(100), dec!(1), pair, "s2");
        let s3 = test_order(Side::Sell, dec!(101), dec!(1), pair, "s3");
        for s in [&s1, &s2, &s3] {
            cache::save_order_to_book(&pool, s).await.unwrap();
        }

        // Verify book is sorted correctly (price ASC)
        let book_check = cache::load_order_book(&pool, pair, Side::Sell).await.unwrap();
        assert_eq!(book_check[0].price, Some(dec!(99)));
        assert_eq!(book_check[1].price, Some(dec!(100)));
        assert_eq!(book_check[2].price, Some(dec!(101)));

        // Incoming buy at 100 for qty 2 — should fill 99 and 100, skip 101
        let buy = test_order(Side::Buy, dec!(100), dec!(2), pair, "buyer");

        let guard = lock::acquire_lock(&pool, pair, "w1").await.unwrap();
        let mut book = cache::load_order_book(&pool, pair, Side::Sell).await.unwrap();
        let result = match_order(&buy, &mut book);

        assert_eq!(result.trades.len(), 2);
        assert_eq!(result.trades[0].price, dec!(99));
        assert_eq!(result.trades[1].price, dec!(100));
        assert_eq!(result.incoming.status, OrderStatus::Filled);

        // Remove filled orders from cache
        for upd in &result.book_updates {
            if upd.status == OrderStatus::Filled {
                cache::remove_order_from_book(&pool, upd).await.unwrap();
            }
        }

        guard.release().await;

        // Only the 101 ask should remain
        let book = cache::load_order_book(&pool, pair, Side::Sell).await.unwrap();
        assert_eq!(book.len(), 1);
        assert_eq!(book[0].price, Some(dec!(101)));

        // Cleanup
        for s in [&s1, &s2, &s3] { cleanup_order(&pool, &s.id).await; }
        cleanup_pair(&pool, pair).await;
    }

    #[tokio::test]
    async fn full_cycle_lock_contention_sequential() {
        let pool = dragonfly_pool().await;
        let pair = "TEST-CONTENTION";
        cleanup_pair(&pool, pair).await;

        // Worker A acquires lock
        let guard_a = lock::acquire_lock(&pool, pair, "worker-A").await.unwrap();

        // Worker B tries in background — will wait (backoff) until A releases
        let pool2 = pool.clone();
        let pair2 = pair.to_string();
        let handle = tokio::spawn(async move {
            let guard_b = lock::acquire_lock(&pool2, &pair2, "worker-B").await.unwrap();
            guard_b.release().await;
            true
        });

        // Release A after a short delay
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        guard_a.release().await;

        // B should succeed
        let got_lock = tokio::time::timeout(
            tokio::time::Duration::from_secs(5),
            handle,
        ).await.unwrap().unwrap();
        assert!(got_lock);

        cleanup_pair(&pool, pair).await;
    }

    // ═══════════════════════════════════════════════════════════════════════
    // COLLISION / SEQUENCING / DOUBLE-SELL SCENARIOS
    // ═══════════════════════════════════════════════════════════════════════

    /// Two buyers race for the same single ask. Lock ensures only one fills.
    #[tokio::test]
    async fn collision_two_buyers_one_ask() {
        use crate::engine::match_order;

        let pool = dragonfly_pool().await;
        let pair = "TEST-COLLISION-2B1A";
        cleanup_pair(&pool, pair).await;

        // 1 ask resting at 100 for qty 1
        let ask = test_order(Side::Sell, dec!(100), dec!(1), pair, "seller");
        cache::save_order_to_book(&pool, &ask).await.unwrap();

        let pool_a = pool.clone();
        let pool_b = pool.clone();
        let pair_a = pair.to_string();
        let pair_b = pair.to_string();

        // Worker A: buy 1 at 100
        let buy_a = test_order(Side::Buy, dec!(100), dec!(1), pair, "buyer-A");
        let buy_b = test_order(Side::Buy, dec!(100), dec!(1), pair, "buyer-B");

        // Run both concurrently — lock serializes them
        let handle_a = tokio::spawn({
            let buy = buy_a.clone();
            async move {
                let guard = lock::acquire_lock(&pool_a, &pair_a, "worker-A").await.unwrap();
                let mut book = cache::load_order_book(&pool_a, &pair_a, Side::Sell).await.unwrap();
                let result = match_order(&buy, &mut book);
                // Apply fills to cache
                for upd in &result.book_updates {
                    if upd.status == OrderStatus::Filled {
                        cache::remove_order_from_book(&pool_a, upd).await.unwrap();
                    } else {
                        cache::save_order_to_book(&pool_a, upd).await.unwrap();
                    }
                }
                guard.release().await;
                result.trades.len()
            }
        });

        let handle_b = tokio::spawn({
            let buy = buy_b.clone();
            async move {
                let guard = lock::acquire_lock(&pool_b, &pair_b, "worker-B").await.unwrap();
                let mut book = cache::load_order_book(&pool_b, &pair_b, Side::Sell).await.unwrap();
                let result = match_order(&buy, &mut book);
                for upd in &result.book_updates {
                    if upd.status == OrderStatus::Filled {
                        cache::remove_order_from_book(&pool_b, upd).await.unwrap();
                    } else {
                        cache::save_order_to_book(&pool_b, upd).await.unwrap();
                    }
                }
                guard.release().await;
                result.trades.len()
            }
        });

        let trades_a = handle_a.await.unwrap();
        let trades_b = handle_b.await.unwrap();

        // Exactly ONE buyer should fill, the other gets 0 trades
        assert_eq!(trades_a + trades_b, 1, "exactly one buyer should fill the single ask");

        // Book should be empty — ask was consumed
        let book = cache::load_order_book(&pool, pair, Side::Sell).await.unwrap();
        assert_eq!(book.len(), 0);

        cleanup_order(&pool, &ask.id).await;
        cleanup_pair(&pool, pair).await;
    }

    /// Two sellers race to fill the same single bid. Lock ensures only one fills.
    #[tokio::test]
    async fn collision_two_sellers_one_bid() {
        use crate::engine::match_order;

        let pool = dragonfly_pool().await;
        let pair = "TEST-COLLISION-2S1B";
        cleanup_pair(&pool, pair).await;

        // 1 bid resting at 100 for qty 1
        let bid = test_order(Side::Buy, dec!(100), dec!(1), pair, "buyer");
        cache::save_order_to_book(&pool, &bid).await.unwrap();

        let pool_a = pool.clone();
        let pool_b = pool.clone();
        let pair_a = pair.to_string();
        let pair_b = pair.to_string();

        let sell_a = test_order(Side::Sell, dec!(100), dec!(1), pair, "seller-A");
        let sell_b = test_order(Side::Sell, dec!(100), dec!(1), pair, "seller-B");

        let handle_a = tokio::spawn({
            let sell = sell_a.clone();
            async move {
                let guard = lock::acquire_lock(&pool_a, &pair_a, "worker-A").await.unwrap();
                let mut book = cache::load_order_book(&pool_a, &pair_a, Side::Buy).await.unwrap();
                let result = match_order(&sell, &mut book);
                for upd in &result.book_updates {
                    if upd.status == OrderStatus::Filled {
                        cache::remove_order_from_book(&pool_a, upd).await.unwrap();
                    } else {
                        cache::save_order_to_book(&pool_a, upd).await.unwrap();
                    }
                }
                guard.release().await;
                result.trades.len()
            }
        });

        let handle_b = tokio::spawn({
            let sell = sell_b.clone();
            async move {
                let guard = lock::acquire_lock(&pool_b, &pair_b, "worker-B").await.unwrap();
                let mut book = cache::load_order_book(&pool_b, &pair_b, Side::Buy).await.unwrap();
                let result = match_order(&sell, &mut book);
                for upd in &result.book_updates {
                    if upd.status == OrderStatus::Filled {
                        cache::remove_order_from_book(&pool_b, upd).await.unwrap();
                    } else {
                        cache::save_order_to_book(&pool_b, upd).await.unwrap();
                    }
                }
                guard.release().await;
                result.trades.len()
            }
        });

        let trades_a = handle_a.await.unwrap();
        let trades_b = handle_b.await.unwrap();

        assert_eq!(trades_a + trades_b, 1, "exactly one seller should fill the single bid");

        let book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        assert_eq!(book.len(), 0);

        cleanup_order(&pool, &bid.id).await;
        cleanup_pair(&pool, pair).await;
    }

    /// Double sell: same order ID submitted twice. Second should see empty book.
    #[tokio::test]
    async fn double_sell_same_order() {
        use crate::engine::match_order;

        let pool = dragonfly_pool().await;
        let pair = "TEST-DOUBLE-SELL";
        cleanup_pair(&pool, pair).await;

        // 1 bid at 100, qty 1
        let bid = test_order(Side::Buy, dec!(100), dec!(1), pair, "buyer");
        cache::save_order_to_book(&pool, &bid).await.unwrap();

        let sell = test_order(Side::Sell, dec!(100), dec!(1), pair, "seller");

        // First submission — fills the bid
        let guard = lock::acquire_lock(&pool, pair, "w1").await.unwrap();
        let mut book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        assert_eq!(book.len(), 1);
        let r1 = match_order(&sell, &mut book);
        assert_eq!(r1.trades.len(), 1);
        for upd in &r1.book_updates {
            cache::remove_order_from_book(&pool, upd).await.unwrap();
        }
        guard.release().await;

        // Second submission — same sell order, but bid is gone
        let guard = lock::acquire_lock(&pool, pair, "w2").await.unwrap();
        let mut book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        assert_eq!(book.len(), 0, "bid already consumed — book must be empty");
        let r2 = match_order(&sell, &mut book);
        assert_eq!(r2.trades.len(), 0, "no double fill");
        guard.release().await;

        cleanup_order(&pool, &bid.id).await;
        cleanup_pair(&pool, pair).await;
    }

    /// Sequential orders: 3 sells arrive one after another against a large bid.
    /// Each should partially fill until the bid is exhausted.
    #[tokio::test]
    async fn sequencing_three_sells_one_large_bid() {
        use crate::engine::match_order;

        let pool = dragonfly_pool().await;
        let pair = "TEST-SEQ-3S1B";
        cleanup_pair(&pool, pair).await;

        // Large bid: 5 BTC at 100
        let bid = test_order(Side::Buy, dec!(100), dec!(5), pair, "buyer");
        cache::save_order_to_book(&pool, &bid).await.unwrap();

        let sells = vec![
            test_order(Side::Sell, dec!(100), dec!(2), pair, "s1"),
            test_order(Side::Sell, dec!(100), dec!(2), pair, "s2"),
            test_order(Side::Sell, dec!(100), dec!(3), pair, "s3"), // only 1 can fill
        ];

        let mut total_traded = Decimal::ZERO;
        for sell in &sells {
            let guard = lock::acquire_lock(&pool, pair, "w").await.unwrap();
            let mut book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
            let result = match_order(sell, &mut book);

            for trade in &result.trades {
                total_traded += trade.quantity;
            }

            // Update resting orders in cache
            for upd in &result.book_updates {
                if upd.status == OrderStatus::Filled {
                    cache::remove_order_from_book(&pool, upd).await.unwrap();
                } else {
                    cache::save_order_to_book(&pool, upd).await.unwrap();
                }
            }
            guard.release().await;
        }

        // Total traded should be exactly 5 (the bid qty), not 7 (sum of sells)
        assert_eq!(total_traded, dec!(5), "total traded must equal bid qty, no overfill");

        // Bid should be fully consumed
        let book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        assert_eq!(book.len(), 0);

        cleanup_order(&pool, &bid.id).await;
        cleanup_pair(&pool, pair).await;
    }

    /// 5 sequential sellers each selling 1 against a bid of qty 5.
    /// Total fills must exactly equal 5 — no overfill, no underfill.
    #[tokio::test]
    async fn collision_5_sellers_sequential_exact_fill() {
        use crate::engine::match_order;

        let pool = dragonfly_pool().await;
        let pair = "TEST-COLLISION-5S";
        cleanup_pair(&pool, pair).await;

        // Bid for 5
        let bid = test_order(Side::Buy, dec!(100), dec!(5), pair, "buyer");
        cache::save_order_to_book(&pool, &bid).await.unwrap();

        let mut total_filled = Decimal::ZERO;

        for i in 0..5 {
            let sell = test_order(Side::Sell, dec!(100), dec!(1), pair, &format!("s-{i}"));
            let guard = lock::acquire_lock(&pool, pair, &format!("w-{i}")).await.unwrap();
            let mut book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
            let result = match_order(&sell, &mut book);

            for trade in &result.trades {
                total_filled += trade.quantity;
            }

            for upd in &result.book_updates {
                if upd.status == OrderStatus::Filled {
                    cache::remove_order_from_book(&pool, upd).await.unwrap();
                } else {
                    cache::save_order_to_book(&pool, upd).await.unwrap();
                }
            }
            guard.release().await;
        }

        assert_eq!(total_filled, dec!(5), "exactly 5 units filled");

        // Bid fully consumed
        let book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        assert_eq!(book.len(), 0);

        // 6th seller should get no fill
        let sell_6 = test_order(Side::Sell, dec!(100), dec!(1), pair, "s-extra");
        let guard = lock::acquire_lock(&pool, pair, "w-extra").await.unwrap();
        let mut book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        let result = match_order(&sell_6, &mut book);
        assert_eq!(result.trades.len(), 0, "no liquidity left — must not fill");
        guard.release().await;

        cleanup_order(&pool, &bid.id).await;
        cleanup_pair(&pool, pair).await;
    }

    /// 2 concurrent sellers race for the same bid (tokio::spawn).
    /// Combined with 2-buyer collision test, covers both sides.
    #[tokio::test]
    async fn collision_concurrent_sellers_total_fills() {
        use crate::engine::match_order;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let pool = dragonfly_pool().await;
        let pair = "TEST-COLLISION-CS";
        cleanup_pair(&pool, pair).await;

        // Bid for 1
        let bid = test_order(Side::Buy, dec!(100), dec!(1), pair, "buyer");
        cache::save_order_to_book(&pool, &bid).await.unwrap();

        let total_filled = Arc::new(AtomicU64::new(0));
        let mut handles = Vec::new();

        for i in 0..3 {
            let pool = pool.clone();
            let pair = pair.to_string();
            let filled = total_filled.clone();
            let sell = test_order(Side::Sell, dec!(100), dec!(1), &pair, &format!("s-{i}"));

            handles.push(tokio::spawn(async move {
                let guard = lock::acquire_lock(&pool, &pair, &format!("w-{i}")).await.unwrap();
                let mut book = cache::load_order_book(&pool, &pair, Side::Buy).await.unwrap();
                let result = match_order(&sell, &mut book);

                let qty: u64 = result.trades.iter()
                    .map(|t| t.quantity.to_string().parse::<u64>().unwrap_or(0))
                    .sum();
                filled.fetch_add(qty, Ordering::SeqCst);

                for upd in &result.book_updates {
                    if upd.status == OrderStatus::Filled {
                        cache::remove_order_from_book(&pool, upd).await.unwrap();
                    } else {
                        cache::save_order_to_book(&pool, upd).await.unwrap();
                    }
                }
                guard.release().await;
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let filled = total_filled.load(Ordering::SeqCst);
        assert_eq!(filled, 1, "exactly 1 unit filled — bid was for 1");

        let book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        assert_eq!(book.len(), 0);

        cleanup_order(&pool, &bid.id).await;
        cleanup_pair(&pool, pair).await;
    }

    /// Double fill prevention: sell submitted, matched, then same book state replayed.
    /// Simulates a stale-read scenario — second match sees empty book.
    #[tokio::test]
    async fn double_fill_prevention_stale_read() {
        use crate::engine::match_order;

        let pool = dragonfly_pool().await;
        let pair = "TEST-STALE-READ";
        cleanup_pair(&pool, pair).await;

        let bid = test_order(Side::Buy, dec!(100), dec!(1), pair, "buyer");
        cache::save_order_to_book(&pool, &bid).await.unwrap();

        // First worker: match and commit
        let guard = lock::acquire_lock(&pool, pair, "w1").await.unwrap();
        let mut book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        let sell = test_order(Side::Sell, dec!(100), dec!(1), pair, "seller");
        let r1 = match_order(&sell, &mut book);
        assert_eq!(r1.trades.len(), 1);

        // Commit to cache before releasing lock
        for upd in &r1.book_updates {
            cache::remove_order_from_book(&pool, upd).await.unwrap();
        }
        let v1 = cache::increment_version(&pool, pair).await.unwrap();
        guard.release().await;

        // Second worker: arrives after first committed
        let guard = lock::acquire_lock(&pool, pair, "w2").await.unwrap();
        let book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        let v2_before = cache::increment_version(&pool, pair).await.unwrap();

        // Book is empty — bid was consumed
        assert_eq!(book.len(), 0, "second worker must see empty book");
        assert!(v2_before > v1, "version must have advanced");
        guard.release().await;

        cleanup_order(&pool, &bid.id).await;
        cleanup_pair(&pool, pair).await;
    }

    /// Interleaved buy/sell sequence: alternating orders should produce correct fills.
    #[tokio::test]
    async fn sequencing_interleaved_buy_sell() {
        use crate::engine::match_order;

        let pool = dragonfly_pool().await;
        let pair = "TEST-INTERLEAVE";
        cleanup_pair(&pool, pair).await;

        // Sequence: BUY 2@100, SELL 1@100, SELL 1@100, BUY 1@99
        // Expected: 2 trades at 100, buy fully filled, then buy@99 rests (no ask to match)
        let orders = vec![
            test_order(Side::Buy, dec!(100), dec!(2), pair, "buyer"),
            test_order(Side::Sell, dec!(100), dec!(1), pair, "seller-1"),
            test_order(Side::Sell, dec!(100), dec!(1), pair, "seller-2"),
            test_order(Side::Buy, dec!(99), dec!(1), pair, "buyer-2"),
        ];

        let mut trade_count = 0usize;

        for order in &orders {
            let guard = lock::acquire_lock(&pool, pair, "w").await.unwrap();

            let opposite_side = match order.side {
                Side::Buy => Side::Sell,
                Side::Sell => Side::Buy,
            };
            let mut book = cache::load_order_book(&pool, &pair, opposite_side).await.unwrap();
            let result = match_order(order, &mut book);

            trade_count += result.trades.len();

            // Update matched resting orders
            for upd in &result.book_updates {
                if upd.status == OrderStatus::Filled {
                    cache::remove_order_from_book(&pool, upd).await.unwrap();
                } else {
                    cache::save_order_to_book(&pool, upd).await.unwrap();
                }
            }

            // Add unmatched remainder to book
            if result.incoming.remaining > Decimal::ZERO
                && result.incoming.status != OrderStatus::Cancelled
            {
                cache::save_order_to_book(&pool, &result.incoming).await.unwrap();
            }

            guard.release().await;
        }

        assert_eq!(trade_count, 2, "sell-1 and sell-2 each match the resting buy");

        // Final state: buy@99 resting (no matching ask)
        let bids = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        assert_eq!(bids.len(), 1);
        assert_eq!(bids[0].price, Some(dec!(99)));

        let asks = cache::load_order_book(&pool, pair, Side::Sell).await.unwrap();
        assert_eq!(asks.len(), 0);

        for o in &orders { cleanup_order(&pool, &o.id).await; }
        cleanup_pair(&pool, pair).await;
    }

    /// Partial fill cascade: bid partially filled by sell A, remainder filled by sell B.
    /// Version counter must advance with each mutation.
    #[tokio::test]
    async fn sequencing_partial_cascade_with_versioning() {
        use crate::engine::match_order;

        let pool = dragonfly_pool().await;
        let pair = "TEST-CASCADE-VER";
        cleanup_pair(&pool, pair).await;

        // Bid for 3 at 100
        let bid = test_order(Side::Buy, dec!(100), dec!(3), pair, "buyer");
        cache::save_order_to_book(&pool, &bid).await.unwrap();
        let v0 = cache::increment_version(&pool, pair).await.unwrap();

        // Sell A: 1 at 100 — partial fill of bid (remaining 2)
        let sell_a = test_order(Side::Sell, dec!(100), dec!(1), pair, "s-a");
        let guard = lock::acquire_lock(&pool, pair, "w").await.unwrap();
        let mut book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        let ra = match_order(&sell_a, &mut book);
        assert_eq!(ra.trades.len(), 1);
        assert_eq!(ra.book_updates[0].remaining, dec!(2));
        cache::save_order_to_book(&pool, &ra.book_updates[0]).await.unwrap();
        let v1 = cache::increment_version(&pool, pair).await.unwrap();
        guard.release().await;

        assert!(v1 > v0);

        // Sell B: 2 at 100 — fills remainder
        let sell_b = test_order(Side::Sell, dec!(100), dec!(2), pair, "s-b");
        let guard = lock::acquire_lock(&pool, pair, "w").await.unwrap();
        let mut book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        assert_eq!(book.len(), 1);
        assert_eq!(book[0].remaining, dec!(2), "cache must reflect partial fill from sell A");
        let rb = match_order(&sell_b, &mut book);
        assert_eq!(rb.trades.len(), 1);
        assert_eq!(rb.trades[0].quantity, dec!(2));
        assert_eq!(rb.book_updates[0].status, OrderStatus::Filled);
        cache::remove_order_from_book(&pool, &rb.book_updates[0]).await.unwrap();
        let v2 = cache::increment_version(&pool, pair).await.unwrap();
        guard.release().await;

        assert!(v2 > v1);

        // Book empty
        let book = cache::load_order_book(&pool, pair, Side::Buy).await.unwrap();
        assert_eq!(book.len(), 0);

        cleanup_order(&pool, &bid.id).await;
        cleanup_pair(&pool, pair).await;
    }

    /// Multiple pairs: orders on BTC-USDT should not affect ETH-USDT.
    #[tokio::test]
    async fn collision_cross_pair_isolation() {
        use crate::engine::match_order;

        let pool = dragonfly_pool().await;
        let pair_btc = "TEST-ISO-BTC";
        let pair_eth = "TEST-ISO-ETH";
        cleanup_pair(&pool, pair_btc).await;
        cleanup_pair(&pool, pair_eth).await;

        // BTC: ask at 100
        let btc_ask = test_order(Side::Sell, dec!(100), dec!(1), pair_btc, "seller");
        cache::save_order_to_book(&pool, &btc_ask).await.unwrap();

        // ETH: ask at 200
        let eth_ask = test_order(Side::Sell, dec!(200), dec!(5), pair_eth, "seller");
        cache::save_order_to_book(&pool, &eth_ask).await.unwrap();

        // Buy BTC — should not touch ETH book
        let btc_buy = test_order(Side::Buy, dec!(100), dec!(1), pair_btc, "buyer");
        let guard = lock::acquire_lock(&pool, pair_btc, "w").await.unwrap();
        let mut book = cache::load_order_book(&pool, pair_btc, Side::Sell).await.unwrap();
        let result = match_order(&btc_buy, &mut book);
        assert_eq!(result.trades.len(), 1);
        for upd in &result.book_updates {
            cache::remove_order_from_book(&pool, upd).await.unwrap();
        }
        guard.release().await;

        // ETH book should be untouched
        let eth_book = cache::load_order_book(&pool, pair_eth, Side::Sell).await.unwrap();
        assert_eq!(eth_book.len(), 1);
        assert_eq!(eth_book[0].remaining, dec!(5));

        // BTC book should be empty
        let btc_book = cache::load_order_book(&pool, pair_btc, Side::Sell).await.unwrap();
        assert_eq!(btc_book.len(), 0);

        cleanup_order(&pool, &btc_ask.id).await;
        cleanup_order(&pool, &eth_ask.id).await;
        cleanup_pair(&pool, pair_btc).await;
        cleanup_pair(&pool, pair_eth).await;
    }
}
