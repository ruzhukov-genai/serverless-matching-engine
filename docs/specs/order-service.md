# Spec: Order Service

## Crate: `crates/order-service`

## Responsibilities

- Manage the full order lifecycle (create, modify, cancel)
- Validate orders against pair configuration (tick/lot size, price bands, min/max)
- Persist orders to PostgreSQL
- Publish orders to Valkey Streams for matching
- Dispatch stat update triggers

## Trigger

- Consumes from Valkey Stream: `stream:orders`
- Or receives requests via API (`crates/api`)

## Order Validation

Before accepting an order:
1. Pair exists and is active
2. Price aligned to tick size (`price % tick_size == 0`)
3. Quantity aligned to lot size (`qty % lot_size == 0`)
4. Quantity within `[min_order_size, max_order_size]`
5. Price within price band (`|price - last_trade| / last_trade <= price_band_pct`)
6. Sufficient balance (available >= locked amount)
7. For market orders: price is `None`, must be IOC or FOK

## Order Modify (Cancel-Replace)

1. Cancel existing resting order (release locked funds)
2. Validate new parameters
3. Create new order with new sequence number
4. Publish to match stream

## Locking

All messages requiring DB writes must use Order Book Locking (see `common-functions.md`).

## Sub-components

### StatDispatcher (Scheduled Task)

- Runs every 1 minute (tokio interval or cron)
- Logic:
  1. Fetch list of active pairs
  2. For each pair: `XADD stream:update_stats {pair_id}`

### StatsUpdater (Stream Consumer)

- Consumes from `stream:update_stats`
- For each message: compute and cache 24h stats (high, low, volume, change)

## TODO

- [ ] Define `update_stats` message schema
- [ ] Define stat computation logic
- [ ] Balance locking/unlocking on order create/cancel
