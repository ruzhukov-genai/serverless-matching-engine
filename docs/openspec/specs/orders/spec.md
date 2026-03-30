# Orders Specification

## Purpose
Order submission, lifecycle management, and query for the matching engine.
Users submit limit and market orders; the gateway validates and dispatches them
for matching. Orders transition through a well-defined status lifecycle.

---

## Requirements

### Requirement: Submit Limit Order
The system SHALL accept a limit order with a user-specified price and quantity.

#### Scenario: Valid limit buy
- GIVEN a user with sufficient USDT balance
- WHEN the user submits a limit buy order with a price and quantity
- THEN the system returns 201 Created with an order ID and status "Queued"
- AND the order enters the matching pipeline

#### Scenario: Limit order without price rejected
- GIVEN a limit order request with no price field
- WHEN the gateway validates the request
- THEN the system returns 400 Bad Request with an error message

#### Scenario: Limit order with zero quantity rejected
- GIVEN a limit order request with quantity = 0
- WHEN the gateway validates the request
- THEN the system returns 400 Bad Request

---

### Requirement: Submit Market Order
The system SHALL accept a market order that executes immediately at best available price.

#### Scenario: Valid market sell
- GIVEN a user with sufficient BTC balance
- WHEN the user submits a market sell order with quantity only (no price)
- THEN the system returns 201 Created with an order ID

#### Scenario: Market order with price rejected
- GIVEN a market order request that includes a price field
- WHEN the gateway validates the request
- THEN the system returns 400 Bad Request

---

### Requirement: Time-in-Force
The system SHALL honor the time-in-force (TIF) field on order submission.

Supported values: GTC, IOC, FOK. Default is GTC.

#### Scenario: GTC order rests in book
- GIVEN a GTC limit order with no matching counterpart
- WHEN matching runs
- THEN the order rests in the order book until filled or cancelled

#### Scenario: IOC order cancelled on partial fill
- GIVEN an IOC limit order that partially matches
- WHEN matching runs
- THEN the matched quantity is filled and the remainder is immediately cancelled

#### Scenario: FOK order cancelled if not fully fillable
- GIVEN a FOK limit order for a quantity larger than available liquidity
- WHEN matching runs
- THEN the entire order is cancelled — no partial fill

---

### Requirement: Self-Trade Prevention (STP)
The system SHALL prevent a user's orders from matching against their own orders.

Modes: None (default), CancelMaker, CancelTaker, CancelBoth.

#### Scenario: STP CancelTaker blocks self-match
- GIVEN a resting maker order from user-1
- WHEN user-1 submits a crossing taker order with stp_mode=CancelTaker
- THEN the taker order is cancelled
- AND the maker order remains in the book

---

### Requirement: Cancel Order
The system SHALL allow a user to cancel a resting order.

#### Scenario: Successful cancellation
- GIVEN a resting GTC order with a known order ID
- WHEN the user sends DELETE /api/orders/{order_id}
- THEN the system returns 200 with status "cancel_queued"
- AND the order is eventually removed from the book

#### Scenario: Cancel all orders for a user
- GIVEN a user with multiple resting orders
- WHEN the user sends DELETE /api/orders?user_id={user_id}
- THEN all resting orders for that user are queued for cancellation

---

### Requirement: Modify Order
The system SHALL support order modification via cancel-replace.

#### Scenario: Modify price
- GIVEN a resting GTC limit order
- WHEN the user sends PUT /api/orders/{order_id} with a new price
- THEN the original order is cancelled and a replacement is expected via a new POST /api/orders
- AND the system returns 200 with status "modify_queued"

---

### Requirement: List Orders
The system SHALL return a user's open orders.

#### Scenario: Query open orders
- GIVEN a user with active orders
- WHEN the user sends GET /api/orders?user_id={user_id}
- THEN the system returns a list of orders with status, price, quantity, and remaining quantity

---

### Requirement: Order Lifecycle Timestamps
The system SHALL record three timestamps per order for latency analysis.

- `received_at` — when the gateway receives the order
- `matched_at` — when the Lua matching script completes
- `persisted_at` — when the PostgreSQL transaction commits (set via NOW() inside INSERT)

#### Scenario: All timestamps present after fill
- GIVEN a matched order
- WHEN queried from the database
- THEN received_at, matched_at, and persisted_at are all non-null
- AND matched_at >= received_at AND persisted_at >= matched_at

---

### Requirement: Client Order ID
The system SHALL accept an optional client_order_id for idempotency and tracking.

#### Scenario: Client order ID propagated
- GIVEN an order submitted with a client_order_id
- WHEN the order is created
- THEN the response includes the same client_order_id

---

### Requirement: Order Field Defaults
The system SHALL apply defaults for optional fields.

- user_id defaults to "user-1" if omitted
- tif defaults to GTC if omitted
- stp_mode defaults to None if omitted
