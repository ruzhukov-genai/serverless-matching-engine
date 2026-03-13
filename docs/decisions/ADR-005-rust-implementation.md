# ADR-005: Rust as Implementation Language

**Status:** Accepted

## Context

The matching engine requires:
- Millions of transactions per second throughput
- Absolute reliability — zero tolerance for crashes, data races, or memory corruption
- Predictable latency — no garbage collection pauses

The initial assumption was Node.js/TypeScript. This is inadequate for the performance and reliability requirements.

## Options Considered

| Language | Throughput | Safety | GC Pauses | Maturity (HFT) |
|----------|-----------|--------|-----------|----------------|
| **Rust** | ★★★★★ | ★★★★★ (compile-time) | None | Growing (Binance, Kraken, Solana) |
| **C++** | ★★★★★ | ★★★ (manual) | None | Decades (CME, LMAX) |
| **Go** | ★★★★ | ★★★★ | Short pauses | Moderate |
| **Java** | ★★★★ | ★★★★ | Tunable | Strong (LMAX Disruptor) |
| **Node.js** | ★★ | ★★★ | Unpredictable | Not suitable |

## Decision

Use **Rust** with the tokio async runtime.

**Key factors:**
- **Zero-cost abstractions** — C++-level performance without runtime overhead
- **Ownership model** — compiler prevents data races, null pointers, use-after-free at compile time
- **No garbage collector** — deterministic, predictable latency
- **Fearless concurrency** — safe multi-threaded code by default
- **Modern tooling** — Cargo, built-in testing, criterion benchmarks
- **Industry adoption** — proven in production at major crypto exchanges

## Core Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime (multi-threaded) |
| `redis` (redis-rs) | Dragonfly/Redis client (async) |
| `deadpool-redis` | Connection pooling |
| `sqlx` | PostgreSQL (async, compile-time query checking) |
| `serde` | Serialization/deserialization |
| `serde_json` | JSON for API/config |
| `rmp-serde` | MessagePack for stream messages (compact, fast) |
| `uuid` | Order/trade IDs |
| `chrono` | Timestamps |
| `tracing` | Structured logging + distributed tracing |
| `criterion` | Benchmarking |
| `tokio-test` | Async test utilities |

## Consequences

**Benefits:**
- Entire classes of bugs eliminated at compile time
- Performance headroom for millions of ops/sec
- Single static binary — trivial to deploy (Docker, Lambda, bare metal)
- Excellent async I/O for cache + DB operations

**Risks:**
- Steeper learning curve for developers new to Rust
- Smaller hiring pool than TypeScript/Java
- Longer initial development time (compiler is strict)

**Mitigation:**
- Strict compiler = fewer runtime bugs = less debugging time overall
- Start with the PoC to build team fluency before scaling
