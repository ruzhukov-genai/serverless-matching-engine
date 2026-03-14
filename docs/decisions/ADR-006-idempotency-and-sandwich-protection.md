# ADR-006: Idempotency and Sandwich Attack Prevention

## Status: Accepted

## Context

Financial APIs must guarantee exactly-once order execution and prevent market manipulation through front-running/sandwich attacks.

## Decision: Idempotency

### Mechanism: `client_order_id`
- Optional field on `POST /api/orders`
- Unique constraint: `(user_id, client_order_id)` in PostgreSQL
- On duplicate submission: returns existing order + trades with `idempotent_replay: true` and HTTP 200 (not 201)
- If key is absent: normal behavior (each request creates a new order)

### Why this approach
- **Industry standard**: Stripe, Coinbase, Binance all use client-provided idempotency keys
- **Atomic enforcement**: PostgreSQL unique index guarantees no duplicate, even under concurrent retries
- **Stateless**: No separate idempotency table — the orders table IS the dedup store
- **No TTL complexity**: Keys persist with the order (orders are permanent records)

### Failure modes handled
| Scenario | Result |
|----------|--------|
| Client retries after timeout | Returns cached result (200) |
| Two concurrent requests, same key | DB unique constraint, one succeeds, other gets cached result |
| Same key, different payload | Returns existing order (payload is NOT re-validated — Stripe model) |
| Missing key | Normal flow, new order created |

## Decision: Sandwich Attack Prevention

### What is a sandwich attack?
An attacker sees a pending large order, front-runs it (pushing price unfavorably), then back-runs to profit. Common in DeFi where pending transactions are visible in the mempool.

### Why our CLOB architecture is inherently resistant

1. **No public mempool**: Orders go directly from client → API → matching engine. There is no queue where pending orders are visible to other participants before execution.

2. **Atomic per-pair locking**: Each order acquires a distributed lock before touching the book. Orders are matched serially — no concurrent reader can observe an in-flight order before it completes.

3. **Price-time priority (FIFO)**: The matching engine processes orders strictly by price (best price first), then by arrival time (earliest first). There is no mechanism for any participant to "jump the queue."

4. **Server-side sequencing**: Orders receive monotonic sequence numbers from PostgreSQL's `BIGSERIAL`. The engine matches against the book snapshot loaded inside the lock — the sequence is deterministic and tamper-proof.

5. **Passive matching**: The matching engine is pure — it takes an incoming order and a book snapshot, returning trades. There is no oracle, no external price feed that can be manipulated. Prices are determined solely by limit orders placed by participants.

### Additional protections implemented

| Protection | How |
|------------|-----|
| **Self-Trade Prevention (STP)** | 4 modes prevent wash trading (CancelTaker, CancelMaker, CancelBoth, None) |
| **Price bands** | Orders rejected if price deviates > 10% from last trade price (configurable `price_band_pct`) |
| **Fencing tokens** | Monotonic counter on lock acquisition detects stale writes if a lock expires mid-operation |
| **Double-check locking** | Cancel/modify operations re-verify order status inside the lock to prevent TOCTOU races |

### What we DON'T need (and why)

| DeFi mechanism | Why not needed |
|----------------|---------------|
| Commit-reveal schemes | No mempool to front-run against |
| Batch auctions | FIFO is the standard for CLOBs; batch auctions add latency |
| Private transaction pools | Orders are already private until matched |
| MEV protection (Flashbots, etc.) | No blockchain, no MEV |

## References
- Stripe idempotency: https://stripe.com/blog/idempotency
- Binance API client_order_id: https://binance-docs.github.io/apidocs/spot/en/
- FINRA front-running rules: https://www.finra.org/
