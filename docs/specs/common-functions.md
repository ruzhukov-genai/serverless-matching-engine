# Spec: Common Functions

## Order Book Locking

Organizes concurrent access and preserves message sequence per order book.

### Redis Keys

| Key | Type | Purpose |
|-----|------|---------|
| `book:{pair_id}:queue` | List | Pending messages for pair |
| `book:{pair_id}:lock` | String (NX EX) | Distributed mutex |
| `book:{pair_id}:version` | Counter | Optimistic versioning |

### Algorithm

```
1. Receive message from Rabbit queue
2. RPUSH book:{pair_id}:queue {message}
3. Obtain Lock:
   a. res = SET book:{pair_id}:lock {randomId} NX EX 1
4. IF res != null (lock acquired):
   a. message = LPOP book:{pair_id}:queue
   b. Execute order book function (match, update, etc.)
   c. Write updated order book to DB
   d. INCR book:{pair_id}:version
   e. Release Lock:
      - res = GET book:{pair_id}:lock
      - IF res == {randomId}: DEL book:{pair_id}:lock
5. ELSE (lock contention):
   a. AdjustedRetryTime = RetryTime[RetryAttempt]  (exponential backoff)
   b. AdjustedRetryTime *= (1 + 0.2 * Random())    (jitter)
   c. Wait AdjustedRetryTime
   d. RetryAttempt += 1
   e. Go to step 3
6. On any error: Release Lock
```

### Notes

- `{randomId}` must be unique per lock holder to prevent accidental release
- Single-node Redis weakness: mitigated via RedLock post-migration
- Backoff list (to define): e.g. `[10ms, 20ms, 50ms, 100ms, 200ms, 500ms]`

### TODO

- [ ] Define `RetryTime` backoff array and max attempts
- [ ] Instrument: lock wait time, contention rate, lock duration
- [ ] RedLock implementation plan
