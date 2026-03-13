# Tests

## Test Categories

### Unit Tests
- Matching logic (price-time priority, partial fills)
- Lock acquire/release/backoff
- Version guard logic

### Integration Tests
- Single pair: submit 2 crossing orders → verify trade
- Multi-pair: concurrent orders on different pairs → verify isolation
- Cancel: cancel resting order → verify removed from book

### Concurrency Tests
- N workers, same pair, rapid-fire orders → verify no duplicates/losses
- Lock contention under load → measure retry rates
- Worker crash simulation → verify lock TTL recovery

### Load Tests
- Throughput: orders/sec (single pair)
- Throughput scaling: orders/sec across 10, 100, 1000 pairs
- Latency percentiles: P50, P95, P99 per match cycle

### Edge Cases
- Partial fill → remaining rests in book
- Order exactly matches full book depth
- Empty book (no match, order rests)
- Self-trade prevention (if applicable)
- Rapid cancel + match race condition
