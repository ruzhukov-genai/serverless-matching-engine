# Spec: Order Service

## Responsibilities

- Manage the full order lifecycle (create, update, cancel)
- Dispatch stat update triggers every minute
- Process stat update messages

## Trigger

- Lambda trigger: single order queue

## Locking

All messages requiring DB writes must use Order Book Locking (see `common-functions.md`).

## Sub-components

### StatDispatcher (Scheduled Lambda)

- Runs every 1 minute (EventBridge cron)
- Entry point: separate handler in same codebase
- Logic:
  1. Fetch list of active pairs
  2. For each pair: `PUBLISH update_stats {pair_id}` to queue

### StatsUpdater (Queue-triggered Lambda)

- Listens to `update_stats` queue
- Entry point: separate handler in same codebase
- Logic: for each message, execute Update Stat for the pair

## Phases

1. Implement Order Book Locking for all write messages
2. Refactor to single order queue
3. Lambda deployment
4. Implement StatDispatcher (scheduled)
5. Implement StatsUpdater (queue-triggered)

## TODO

- [ ] Define order schema (fields, status states, versioning)
- [ ] Define `update_stats` message schema
- [ ] Define stat computation logic
- [ ] EventBridge rule for StatDispatcher (1-min schedule)
