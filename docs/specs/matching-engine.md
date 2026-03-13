# Spec: Matching Engine

## Crate: `crates/matching-engine`

## Stateless Cycle

```rust
loop {
    let order = stream.xreadgroup("stream:orders:match").await;
    let lock = acquire_lock(&pair_id, worker_id, Duration::from_secs(1)).await;

    let book = load_order_book(&pair_id, order.side.opposite()).await;
    let result = match_order(&order, &mut book);

    write_book_updates(&pair_id, &result.book_updates).await;
    increment_version(&pair_id).await;
    publish_trades(&result.trades).await;
    publish_audit_events(&result).await;
    persist_async(&result).await;

    release_lock(&pair_id, worker_id).await;
    stream.xack("stream:orders:match", &order.id).await;
}
```

## Order Types

### Limit Order
- Has explicit price
- **BUY at $P**: match against asks where `price <= P`, lowest first, then oldest
- **SELL at $P**: match against bids where `price >= P`, highest first, then oldest
- Unmatched remainder behavior depends on TIF

### Market Order
- No price — takes best available
- Walks the book until fully filled or book exhausted
- Never rests in book (implicit IOC behavior)
- Rejected if book is empty

## Time-in-Force (TIF)

| TIF | Behavior after matching |
|-----|----------------------|
| **GTC** | Unfilled remainder rests in book |
| **IOC** | Cancel unfilled remainder immediately |
| **FOK** | If not fully filled, reject entire order (no trades) |

- Market orders: always IOC (never rest)
- FOK requires pre-check: sufficient liquidity at compatible prices before executing

## Self-Trade Prevention (STP)

When incoming order would match against a resting order from the same `user_id`:

| Mode | Action |
|------|--------|
| `None` | Allow the self-trade |
| `CancelTaker` | Cancel the incoming order, keep resting |
| `CancelMaker` | Cancel the resting order, continue matching |
| `CancelBoth` | Cancel both orders |

Emit `AuditEvent::SelfTradePrevented` for every STP trigger.

## Partial Fills

- Reduce `remaining` on both sides
- Update `status` to `PartiallyFilled`
- When `remaining == 0`: status = `Filled`, remove from book

## Lock Contention

```rust
const BACKOFF: &[u64] = &[10, 20, 50, 100, 200, 500]; // ms
const MAX_RETRIES: usize = 10;

fn jitter(delay: u64) -> u64 {
    delay + (delay as f64 * 0.2 * rand::random::<f64>()) as u64
}
```

If max retries exceeded: fail, requeue the message, emit alert metric.

## Order Book Cache

```
book:{pair_id}:bids  → ZSet (score = price, member = order_id)
book:{pair_id}:asks  → ZSet (score = price, member = order_id)
order:{order_id}     → Hash {price, quantity, remaining, side, pair_id,
                              user_id, order_type, tif, stp_mode,
                              created_at, version}
```

## Audit Events Emitted

- `OrderCreated` — new order accepted
- `TradeExecuted` — for each fill
- `OrderCancelled` — IOC remainder, FOK rejection, STP cancellation
- `SelfTradePrevented` — STP triggered

## What We Need to Prove

- [ ] Correct matching under concurrent load (same pair)
- [ ] No fills beyond order quantity
- [ ] Market orders execute correctly (walk full book)
- [ ] FOK: all-or-nothing (no partial trades leak)
- [ ] IOC: unfilled remainder canceled, partial fills kept
- [ ] STP: all modes work correctly
- [ ] Version conflicts handled (stale write rejected)
- [ ] Lock expiry recovery (worker crash mid-match)
- [ ] Throughput: matches/sec single pair + multi-pair scaling
