# Spec: Common Functions

## Crate: `crates/shared`

## Order Book Locking

Organizes concurrent access and preserves message sequence per order book.
Uses Valkey (Redis-compatible) distributed locking.

### Valkey Keys

| Key | Type | Purpose |
|-----|------|---------|
| `book:{pair_id}:queue` | List | Pending messages for pair |
| `book:{pair_id}:lock` | String (NX EX) | Distributed mutex |
| `book:{pair_id}:version` | Counter | Optimistic versioning |

### Algorithm

```
1. Receive message from Valkey Stream
2. RPUSH book:{pair_id}:queue {message}
3. Obtain Lock:
   a. res = SET book:{pair_id}:lock {worker_id} NX EX 1
4. IF res != null (lock acquired):
   a. message = LPOP book:{pair_id}:queue
   b. Execute order book function (match, update, etc.)
   c. Write updated order book to DB
   d. INCR book:{pair_id}:version
   e. Release Lock:
      - res = GET book:{pair_id}:lock
      - IF res == {worker_id}: DEL book:{pair_id}:lock
5. ELSE (lock contention):
   a. AdjustedRetryTime = BACKOFF[attempt]  (exponential: [10, 20, 50, 100, 200, 500] ms)
   b. AdjustedRetryTime *= (1 + 0.2 * random())  (jitter)
   c. Wait AdjustedRetryTime
   d. attempt += 1
   e. If attempt > MAX_RETRIES (10): fail + requeue
   f. Go to step 3
6. On any error: Release Lock
```

### Notes

- `{worker_id}` must be unique per lock holder to prevent accidental release
- Single-node Valkey weakness: mitigated via RedLock post-migration
- Lock TTL (1 second) ensures crashed workers do not permanently block a pair

### TODO

- [ ] Instrument: lock wait time, contention rate, lock duration
- [ ] RedLock implementation plan (multi-node Valkey)
