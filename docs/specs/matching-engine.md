# Spec: Matching Engine

## Responsibilities

- Match incoming orders against the resting order book
- Manage Order Book state (load, update, persist)
- Emit trade events to Transaction Service

## Trigger

- Lambda trigger: SQS / RabbitMQ queue (single consolidated queue — see ADR-003)

## Stateless Mode

Controlled by `STATELESS_MODE` env var / CDK parameter.

When enabled:
1. Load Order Book on invocation (Redis → DB fallback)
2. Match order
3. Finalize all DB writes before returning

## Order Book Loading Optimizations

### Price-filtered loading
- Limit BUY at $P → load only Sell orders where `price <= P`
- Limit SELL at $P → load only Buy orders where `price >= P`

### Lazy loading
```
Batch 1:  10 orders
Batch 2:  20 orders  (if batch 1 fully matched)
Batch 3:  40 orders
...
```

### Redis cache
- Write-through cache; source of truth for active orders
- Sorted sets per side: `book:{pair_id}:bids` / `book:{pair_id}:asks` (sorted by price)

## Order Persistence

- Synchronous write-through to Redis
- Async DB write (version-guarded: only write if `order.version > db.version`)

## Phases

1. Load test baseline
2. Implement `StatelessMode` option
3. Performance analysis & optimization (loading + saving)
4. Implement Order Book Locking
5. Refactor to single instance listening to all pair queues
6. Refactor to single consolidated queue
7. Lambda deployment

## TODO

- [ ] Define order book data model
- [ ] Define trade/fill event schema
- [ ] Define `StatelessMode` CDK parameter
- [ ] Load test harness design
