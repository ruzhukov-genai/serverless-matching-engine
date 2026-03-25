# Optimization Log — serverless-matching-engine

Complete history of optimization attempts, results, and lessons. 
**Check this before proposing new optimizations to avoid repeating failures.**

---

## Baseline (Mar 14, pre-optimization)

```
@100 clients:
Orders/sec:    0 (100% errors — pool exhaustion)
Orderbook:     14,813ms
Ticker:        4,221ms
Metrics:       20,377ms
```

---

## Round 1 — REST + Cache Fixes (Mar 14, commits 10da63a..c79cea4)

### ✅ Successes
| Optimization | Before | After | Impact |
|---|---|---|---|
| Shared WS broadcast (1 poller per pair, fan-out) | 200 WS conns each polling DF | 1 poller per key | Orders went from 0 to 441 (29/s) |
| Top-200 orderbook batching | 14k HMGET per load | 200 per side | Orderbook 14,813ms → 725ms (20x) |
| Metrics in-memory cache (5s refresh) | Per-request DF reads | Background cache | 20,377ms → 112ms (182x) |
| Valkey pool 50→200 | Pool exhaustion | Room for growth | 100% errors → 0% |
| WS /orders (pub/sub) | REST polling | Real-time push | Removed REST polling entirely |
| PG hot pool 30→60 | Connection pressure | Headroom | Enabled concurrent order processing |

### Result: 0 → 82.1 ord/s, 0% → 0% errors

---

## Round 2 — Worker Hot Path (Mar 15, commits 0e7c0f1..2d64807)

### ✅ Successes
| Optimization | Before | After | Impact |
|---|---|---|---|
| Per-pair queue consumers (BRPOP per pair) | Single BRPOP loop | N pair consumers | Parallel processing per pair |
| Dirty-user portfolio (mpsc channel) | Scan ALL users every 2s | Only dirty users | Eliminated N*users DB queries |
| Parallel pre-match DB (try_join!) | Sequential insert+lock | Parallel | ~2ms saved |
| Orderbook rebuild skip (no-op check) | Rebuild every order | Skip when no change | Eliminated wasted DF reads |
| Price-bounded Lua fetch | Fetch all resting | LIMIT 50, price-bounded | Less data in Lua |
| Cache update off hot path (tokio::spawn) | 66% of hot path | 0% (fire-and-forget) | 13.5ms → 0ms on hot path |
| insert_order_db deferred to persist worker | Pre-match DB write | Post-match async | ~2ms saved |
| Lua: 2 round-trips → 1 (ZRANGEBYSCORE inside Lua) | 2 DF round-trips | 1 EVAL | ~1ms saved |
| Semaphore(3) per pair + batch drain | Unlimited concurrent | Bounded concurrency | Prevented PG row-lock contention |

### Result: Hot path median 20,491µs → 5,384µs (-74%)

---

## Round 3 — Gateway + Watch Channels (Mar 15, commit a26dca1)

### ✅ Successes
| Optimization | Before | After | Impact |
|---|---|---|---|
| CacheBroadcasts RwLock → watch channels | RwLock serialization | Zero-contention reads | REST unblocked |
| Removed duplicate refresh workers | 3 overlapping workers | 1 unified worker | Less CPU/DF ops |
| Portfolio full-scan eliminated | All users every 2s | Dirty-only via mpsc | Huge DB savings |
| Batched metrics (atomic counters, 2s flush) | 4 DF writes per order | Batch every 2s | DF ops ~400/s → ~3/s |
| Debounced orderbook rebuild (10ms window) | Rebuild per order | Coalesce per pair | Eliminated redundant rebuilds |

### Result: 93.8 → 138.3 ord/s (+47%), p95 949ms → 364ms (-62%)

---

## Round 4 — Gateway Minor (Mar 15, commit ddce9f2)

### ✅ Successes
| Optimization | Impact |
|---|---|
| Raw JSON responses (skip parse/reserialize) | Neutral at 100, small win at 50 |
| Arc\<str\> in watch channels | Zero-alloc reads |
| Gateway DF pool 200→30 | Reduced DF pressure |
| std::sync::RwLock for UserCache | Faster than tokio RwLock for non-async |

### ❌ Failures / Reverted
| Optimization | Why it failed |
|---|---|
| **CompressionLayer (gzip)** | CPU cost > benefit on 2-vCPU. Small cached JSON compresses poorly. |
| **worker_threads=8** | Context-switch regression on 2-vCPU. Oversubscribing tokio hurts. |

### Lesson: On 2-vCPU, don't oversubscribe threads. Gzip hurts for small JSON.

---

## Round 5 — Pub/Sub + Balance Lock (Mar 15, commit 3ed4006)

### ✅ Successes
| Optimization | Before | After | Impact |
|---|---|---|---|
| Pub/Sub replaces 15 pollers | 15 per-key pollers | 1 SUBSCRIBE | DF connections -55%, data staleness 5s → <1ms |
| WS broadcast throttle (10/sec per key) | Unlimited sends | Max 10/sec | 32% fewer WS messages |
| Orderbook snapshot via single Lua EVAL | 100+ round-trip rebuild | 1 EVAL | Massive rebuild speedup |
| lock_balance via DF pipeline (HGET+HINCRBY) | PG round-trip (4.5ms) | DF pipeline (<0.5ms) | ~4ms saved per order |

### ❌ Failures / Reverted
| Optimization | Why it failed |
|---|---|
| **lock_balance as Lua EVAL** | Made things WORSE (6ms vs 4.5ms PG). Valkey Lua is single-threaded — balance lock EVAL competed with matching EVAL. |
| **Semaphore sem=6 (increased from 3)** | More concurrent orders = more Valkey Lua contention. Reverted to sem=3. |

### 🔴 CRITICAL LESSON: Valkey Lua contention
**Any new Lua EVAL on the hot path will degrade matching performance.** Valkey runs Lua scripts single-threaded. Adding more EVALs = serialization = slower. Use pipeline commands (non-blocking) instead of EVAL when possible.

---

## Round 6 — New Features (Mar 16, commits 3ca09a1..a46e39a)

### ✅ Successes
| Optimization | Impact |
|---|---|
| Batch snapshot API (`/api/snapshot/{pair_id}`) | Single request replaces 8 REST calls |
| Multiplexed WS (`/ws/stream`) | 1 WS connection replaces 3+ per client |
| TCP_NODELAY on all connections | Free latency win for small JSON |
| Valkey Unix Domain Socket | ~18% latency improvement over TCP loopback |

### ❌ Failures / Reverted
| Optimization | Why it failed |
|---|---|
| **SO_REUSEPORT** | Needs multiple accept loops to help. Single accept loop adds overhead. Reverted. |
| **HTTP/2 (h2c)** | Head-of-line blocking causes massive latency regression. At 100 clients: h2 avg 1940ms vs h1.1 avg 69ms. HTTP/1.1 + many connections wins for this workload. |

### Result: 138.2 → 161.7 ord/s (+17%), avg 129ms → 69ms (-47%)

---

## Round 7 — Merged Lua + Micro-optimizations (Mar 20, commit 43c424b)

### ✅ Successes
| Optimization | Impact |
|---|---|
| Merge balance lock INTO match_order.lua | Eliminates 1 full DF round-trip (~1.5ms). Now lock+match in single EVAL. |
| Dedicated BRPOP connection (no pool checkout) | Avoids deadpool mutex per iteration |
| Pre-computed PairKeys (format! eliminated) | 3 allocations removed per order |
| Skip WS serialization when no subscribers | trade_to_json + serde skipped when idle |
| Sampled tracing (debug per order, info every 100th) | Reduces tracing overhead at high throughput |
| Arc\<Order\> in PersistJob | Atomic refcount vs deep String clone |

### Note on Lua contention
Round 5 showed that a SEPARATE Lua EVAL for balance lock competed with matching and made things worse. Round 7's approach is different: it MERGES the balance lock INTO the existing matching Lua script, so there's still only 1 EVAL (not 2). This works because it doesn't add a new script, just extends the existing one.

### Result: 138.2 → 168.2 ord/s (+22%), avg 128.7ms → 56.7ms (-56%), p95 346ms → 171ms (-51%)

---

## Cumulative Results (all rounds)

```
                  Original(Mar14)   Final(Mar20)   Change
Orders/sec @100   0 (broken)        168.2          ∞
Order avg @100    N/A               56.7ms         —
Order p95 @100    N/A               171.0ms        —
Orderbook REST    14,813ms          ~90ms          165x
Metrics REST      20,377ms          ~57ms          357x
WS messages       182               16,820         92x
DF clients @100   exhausted         46             —
Data staleness    5,000ms           <1ms           5000x
```

---

## Known Ceiling

**At 100 clients on 2-vCPU:** 1006 TCP connections saturate OS TCP scheduling. This is the hard limit — not app code.

Options to push further:
- Bigger instance (4+ vCPU)
- HTTP/2 multiplexing (reduces TCP count but has HOL blocking tradeoff)
- Connection pooling / keep-alive tuning

---

## Anti-Patterns (Don't Try Again)

1. **❌ Separate Lua EVAL for balance operations** — competes with matching Lua. Merge into existing script or use pipeline.
2. **❌ CompressionLayer / gzip** — CPU > benefit on 2-vCPU for small JSON.
3. **❌ worker_threads > num_cpus** — context-switch overhead on small instances.
4. **❌ SO_REUSEPORT with single accept loop** — needs multiple listeners to help.
5. **❌ HTTP/2 for many-small-response workloads** — stream HOL blocking kills latency.
6. **❌ Semaphore > 3 per pair** — increases PG row-lock contention + Lua contention.
7. **❌ Parallel sub-agent cargo builds on 2-vCPU** — target dir contention, all time out.
