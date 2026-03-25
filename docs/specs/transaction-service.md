# Spec: Transaction Service

## Crate: `crates/transaction-service`

## Responsibilities

- Consume trade events from Matching Engine
- Persist transactions to PostgreSQL
- Update user balances (debit buyer, credit seller)
- Publish trade confirmations

## Trigger

- Consumes from Valkey Stream: `stream:transactions`

## Trade Processing

For each trade event:
1. Validate trade (order IDs exist, quantities match)
2. Begin DB transaction:
   - Insert trade record
   - Debit buyer: reduce quote asset, add base asset
   - Credit seller: reduce base asset, add quote asset
   - Release locked funds for both sides
3. Commit transaction
4. Publish confirmation to `stream:trade_confirmations` (for WebSocket push)

## Idempotency

- Each trade has a unique `trade_id` (UUID)
- Insert with `ON CONFLICT DO NOTHING`
- Prevents double-processing from stream redelivery

## Locking

All messages requiring DB writes must use Order Book Locking (see `common-functions.md`).

## TODO

- [ ] Define balance update SQL (atomic debit/credit)
- [ ] Define idempotency key strategy
- [ ] Define dead-letter handling for failed trades
