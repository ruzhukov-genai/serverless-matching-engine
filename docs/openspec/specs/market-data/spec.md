# Market Data Specification

## Purpose
Real-time and snapshot market data feeds: order book depth, recent trades,
ticker stats, and pair configuration. All market data is served from Valkey
cache — zero database reads on the hot path.

---

## Requirements

### Requirement: Order Book Depth
The system SHALL provide aggregated bid/ask levels for a trading pair.

Response format: `{ "bids": [[price, qty], ...], "asks": [[price, qty], ...] }`
Bids sorted descending by price; asks sorted ascending by price.

#### Scenario: Non-empty book returned
- GIVEN orders resting in the BTC-USDT book
- WHEN GET /api/orderbook/BTC-USDT is called
- THEN bids and asks are returned sorted by price

#### Scenario: Empty book returns empty arrays
- GIVEN no orders in the book
- WHEN GET /api/orderbook/BTC-USDT is called
- THEN response is {"bids":[], "asks":[]}

#### Scenario: Depth parameter limits levels
- GIVEN a book with 100 price levels
- WHEN GET /api/orderbook/BTC-USDT?depth=10 is called
- THEN at most 10 bids and 10 asks are returned

---

### Requirement: Recent Trades
The system SHALL provide the most recent trades for a pair.

#### Scenario: Trades returned after a match
- GIVEN a completed match for BTC-USDT
- WHEN GET /api/trades/BTC-USDT is called
- THEN at least one trade appears with price, quantity, and timestamp

#### Scenario: Empty trades on no activity
- GIVEN no matches for a pair
- WHEN GET /api/trades/{pair_id} is called
- THEN response is {"trades":[]}

---

### Requirement: Ticker
The system SHALL provide 24-hour ticker statistics for a pair.

Fields: last price, 24h high, 24h low, 24h volume, price change.

#### Scenario: Ticker populated after trade
- GIVEN a completed trade for BTC-USDT
- WHEN GET /api/ticker/BTC-USDT is called
- THEN last price matches the most recent trade price

#### Scenario: Ticker null before any trade
- GIVEN no trades have occurred for a pair
- WHEN GET /api/ticker/{pair_id} is called
- THEN last, high_24h, low_24h, volume_24h are all null

---

### Requirement: Trading Pairs
The system SHALL return the list of configured trading pairs.

#### Scenario: Pairs list non-empty
- GIVEN the system is initialized with pairs BTC-USDT, ETH-USDT, SOL-USDT
- WHEN GET /api/pairs is called
- THEN all three pairs are returned with their configuration

---

### Requirement: Snapshot Endpoint
The system SHALL provide a single composite snapshot endpoint that returns
orderbook, trades, ticker, metrics, throughput, latency, and pairs in one response.
This replaces 7+ individual REST calls.

#### Scenario: Snapshot returns all data in one call
- GIVEN the system is running with activity
- WHEN GET /api/snapshot/{pair_id} is called
- THEN a single JSON object with keys: orderbook, trades, ticker, metrics,
  throughput, latency, pairs is returned

#### Scenario: Snapshot respects depth parameter
- GIVEN GET /api/snapshot/BTC-USDT?depth=5
- WHEN the response is returned
- THEN orderbook.bids and orderbook.asks are each limited to 5 levels

---

### Requirement: Cache Freshness
Market data responses SHALL reflect the most recent Valkey state.

Orderbook reads go directly to Valkey (authoritative).
Ticker, trades, and pairs are served from in-memory CacheBroadcasts (eventual consistency, <100ms lag acceptable).

#### Scenario: Orderbook reflects latest match
- GIVEN a match just occurred
- WHEN GET /api/orderbook/{pair_id} is called within 1 second
- THEN the matched orders are no longer visible in the book

---

### Requirement: Dashboard Metrics
The system SHALL expose internal performance metrics for the admin dashboard.

Endpoints:
- GET /api/metrics — order processing metrics
- GET /api/lock-metrics — distributed lock stats
- GET /api/throughput — time-series throughput data
- GET /api/latency-percentiles — p50/p95/p99 latency breakdown

#### Scenario: Metrics populated during load
- GIVEN the system is processing orders
- WHEN GET /api/metrics is called
- THEN non-empty metrics JSON is returned
