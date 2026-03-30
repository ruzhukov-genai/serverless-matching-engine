# Portfolio Specification

## Purpose
User balances and portfolio view. Balances reflect available and locked amounts
per asset. The primary cache is Valkey; PostgreSQL is the source of truth.

---

## Requirements

### Requirement: Portfolio Query
The system SHALL return a user's current balances by asset.

Response includes: asset, available, locked for each held asset.

#### Scenario: Portfolio returned for known user
- GIVEN user-1 has BTC and USDT balances
- WHEN GET /api/portfolio?user_id=user-1 is called
- THEN a list of balances with available and locked amounts is returned

#### Scenario: Empty portfolio for unknown user
- GIVEN a user with no balance records
- WHEN GET /api/portfolio?user_id={user_id} is called
- THEN response is {"balances":[]}

#### Scenario: Default user applied when user_id omitted
- GIVEN a request to GET /api/portfolio with no user_id query param
- WHEN the gateway processes the request
- THEN it defaults to user_id=user-1

---

### Requirement: Available vs Locked Balance
The system SHALL maintain separate available and locked balances per (user, asset).

- available: spendable balance
- locked: reserved for open orders

#### Scenario: Lock reduces available, increases locked
- GIVEN user-1 has 1000 USDT available
- WHEN a buy order for 500 USDT is placed
- THEN available = 500, locked = 500

#### Scenario: Fill releases locked, transfers asset
- GIVEN user-1 has 500 USDT locked (from an open buy)
- WHEN the buy order is fully filled
- THEN USDT locked is released and BTC available is credited

---

### Requirement: Balance Initialization
The system SHALL seed initial balances for test users on reset.

Default seeds: 10 BTC and 1,000,000 USDT per user.

#### Scenario: reset_all seeds balances
- GIVEN a POST /internal/manage with command=reset_all and users=100
- WHEN the command executes
- THEN users user-1 through user-100 each have 10 BTC and 1,000,000 USDT available

---

### Requirement: Real-Time Portfolio Updates via WS
The system SHALL push portfolio updates to subscribed clients when balances change.

#### Scenario: Portfolio pushed after fill
- GIVEN a client subscribed to portfolio:user-1 via multiplexed WS
- WHEN user-1's order is filled and balances change
- THEN an updated portfolio payload is pushed within 2 seconds
