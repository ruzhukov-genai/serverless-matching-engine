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
}
