# Spec: Transaction Service

## Responsibilities

- Receive trade/fill events from Matching Engine
- Persist transactions to DB
- Update account balances

## Trigger

- Lambda trigger: transaction queue

## Locking

All messages requiring DB writes must use Order Book Locking (see `common-functions.md`).

## Phases

1. Implement Order Book Locking for all write messages
2. Lambda deployment

## TODO

- [ ] Define transaction schema
- [ ] Define balance update logic and idempotency guarantees
- [ ] Define dead-letter queue (DLQ) handling
