# Matching Engine Specification

## Purpose
The core matching algorithm: how orders are matched, what prices are used,
and what atomicity guarantees are provided. Matching is atomic via Lua EVAL
in Valkey — no distributed lock needed (ADR-004).

---

## Requirements

### Requirement: Price-Time Priority (FIFO)
The system SHALL match orders using price-time priority.

- Best price first: highest bid matches first; lowest ask matches first
- Equal price: earliest order (by insertion time) matches first

#### Scenario: Two bids at same price — earlier wins
- GIVEN two buy orders at price 100.00, submitted at T1 and T2 (T1 < T2)
- WHEN a sell order arrives at price 100.00
- THEN the T1 buy order is matched first

#### Scenario: Higher bid takes priority
- GIVEN a bid at 99.00 and a bid at 100.00
- WHEN a sell order arrives at price 99.00 or below
- THEN the 100.00 bid matches first

---

### Requirement: Limit Order Matching
The system SHALL match a limit buy at or above the best ask price.
The system SHALL match a limit sell at or below the best bid price.

#### Scenario: Limit buy crosses the spread
- GIVEN a resting ask at 100.00
- WHEN a limit buy arrives at price 101.00
- THEN a match occurs at the maker's price (100.00)

#### Scenario: Limit buy does not cross
- GIVEN a resting ask at 100.00
- WHEN a limit buy arrives at price 99.00
- THEN no match occurs and the buy rests in the book

---

### Requirement: Market Order Matching
The system SHALL execute a market order against the best available opposite-side price.
A market order SHALL NOT rest in the book.

#### Scenario: Market sell fills against best bid
- GIVEN a resting bid at 100.00
- WHEN a market sell arrives
- THEN it fills at 100.00

#### Scenario: Market order with no liquidity cancelled
- GIVEN an empty order book
- WHEN a market order arrives
- THEN the order is cancelled (no fill possible)

---

### Requirement: Partial Fills
The system SHALL support partial fills for GTC limit orders.

#### Scenario: Partial fill — remainder rests
- GIVEN a bid for quantity 10 and an ask for quantity 3
- WHEN matching runs
- THEN a trade of quantity 3 occurs
- AND the bid remains in the book with remaining quantity 7

---

### Requirement: Atomic Matching via Lua
The system SHALL execute all book mutations (match, place, cancel) as a single
atomic Lua EVAL script in Valkey. No other process can read a partially-updated
book state.

#### Scenario: Concurrent orders — no partial book state visible
- GIVEN two concurrent orders for the same pair
- WHEN both arrive at roughly the same time
- THEN each sees a consistent book state (Lua serializes access)
- AND no double-fill or missed match occurs

---

### Requirement: Version Counter
The system SHALL increment a per-pair version counter (version:{pair_id}) inside
the Lua script after every book mutation.

#### Scenario: Version increments on match
- GIVEN a pair with version=5
- WHEN an order match occurs
- THEN version becomes 6

#### Scenario: Version increments on order placement
- GIVEN a pair with version=5
- WHEN a resting GTC order is placed in the book
- THEN version becomes 6

---

### Requirement: Inline Dispatch Mode (Production)
In production (ORDER_DISPATCH_MODE=inline), the gateway SHALL execute matching
in-process without invoking a separate Lambda.

#### Scenario: Inline mode — end-to-end in single Lambda
- GIVEN ORDER_DISPATCH_MODE=inline
- WHEN a POST /api/orders request arrives
- THEN matching, balance lock, and DB persist all complete before 201 is returned

---

### Requirement: Per-Pair Isolation
The system SHALL isolate matching per trading pair. Orders for BTC-USDT SHALL
NOT interact with orders for ETH-USDT.

#### Scenario: Cross-pair isolation
- GIVEN a bid for BTC-USDT at 100
- WHEN a sell for ETH-USDT arrives
- THEN no match occurs between them

---

### Requirement: Trade Record
The system SHALL persist every match as a trade record with both sides.

Each trade SHALL include:
- trade_id (UUID)
- pair_id
- buy_order_id
- sell_order_id
- price (maker's price)
- quantity (matched quantity)
- matched_at timestamp

---

### Requirement: Balance Locking
The system SHALL lock the appropriate balance before an order is placed.

- Buy order: lock `quantity * price` USDT from available
- Sell order: lock `quantity` base asset from available

#### Scenario: Insufficient balance rejected
- GIVEN a user with 100 USDT available
- WHEN the user submits a buy for 200 USDT worth at market
- THEN the order is rejected with an insufficient balance error

#### Scenario: Balance locked on order placement
- GIVEN a user with 1000 USDT available
- WHEN a GTC buy order for 500 USDT is placed
- THEN available balance drops to 500 and locked balance increases to 500

#### Scenario: Balance released on fill
- GIVEN a locked balance from a buy order
- WHEN the order is fully filled
- THEN the locked amount is released and the acquired asset is credited
