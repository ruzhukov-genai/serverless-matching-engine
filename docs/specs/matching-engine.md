# Spec: Matching Engine

## Crate: `crates/matching-engine`

## Stateless Cycle

```rust
// Pseudocode
loop {
    let order = stream.xreadgroup("stream:orders:match").await;
    let lock = acquire_lock(&pair_id, worker_id, Duration::from_secs(1)).await;

    let book = load_order_book(&pair_id, order.side.opposite()).await;
    let trades = match_order(&order, &mut book);

    write_book_updates(&pair_id, &book).await;
    increment_version(&pair_id).await;
    publish_trades(&trades).await;
    persist_async(&order, &trades).await;

    release_lock(&pair_id, worker_id).await;
    stream.xack("stream:orders:match", &order.id).await;
}
```

## Matching Logic (Limit Orders)

- **BUY at $P**: match against asks where `price <= P`, lowest first, then oldest
- **SELL at $P**: match against bids where `price >= P`, highest first, then oldest
- Partial fills: reduce remaining quantity, keep in book
- Full fill: remove from book

## Lock Contention

```rust
const BACKOFF: &[u64] = &[10, 20, 50, 100, 200, 500]; // ms
const MAX_RETRIES: usize = 10;

fn jitter(delay: u64) -> u64 {
    delay + (delay as f64 * 0.2 * rand::random::<f64>()) as u64
}
```

## Order Book Cache

```
book:{pair_id}:bids  → ZSet (score = price, member = order_id)
book:{pair_id}:asks  → ZSet (score = price, member = order_id)
order:{order_id}     → Hash {price, quantity, remaining, side, pair_id, created_at, version}
```

## What We Need to Prove

- [ ] Correct matching under concurrent load (same pair)
- [ ] No fills beyond order quantity
- [ ] Version conflicts handled (stale write rejected)
- [ ] Lock expiry recovery (worker crash mid-match)
- [ ] Throughput: matches/sec single pair + multi-pair scaling
