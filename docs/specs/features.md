# Feature Spec: Matching Engine

Complete feature list for the matching engine, organized by priority tier.

## Tier 1 — Core (PoC must-have)

### Order Types
| Type | Description |
|------|-------------|
| **Limit** | Buy/sell at specified price or better |
| **Market** | Buy/sell immediately at best available price |

### Time-in-Force (TIF)
| TIF | Description |
|-----|-------------|
| **GTC** | Good Til Canceled — rests in book until filled or canceled |
| **IOC** | Immediate or Cancel — fill what you can, cancel the rest |
| **FOK** | Fill or Kill — fill entire quantity or cancel completely |

### Matching Algorithm
- **Price-Time Priority (FIFO)** — best price first, earliest order first at same price

### Order Management
| Operation | Description |
|-----------|-------------|
| **Create** | Submit new order |
| **Cancel** | Cancel resting order |
| **Modify** | Amend price and/or quantity of resting order (cancel-replace) |

### Order Book
- Maintain bid/ask sides per pair as sorted price levels
- Each price level: FIFO queue of orders
- Partial fills: reduce remaining quantity, keep in book
- Full fills: remove from book

### Pair Configuration
| Parameter | Description |
|-----------|-------------|
| **Tick size** | Minimum price increment (e.g., 0.01) |
| **Lot size** | Minimum quantity increment (e.g., 0.001) |
| **Min order size** | Minimum quantity per order |
| **Max order size** | Maximum quantity per order |
| **Price precision** | Decimal places for price |
| **Quantity precision** | Decimal places for quantity |
| **Status** | Active / halted / delisted |

### Safety & Protection
| Feature | Description |
|---------|-------------|
| **Self-Trade Prevention (STP)** | Prevent same-user orders from matching. Modes: cancel-taker, cancel-maker, cancel-both |
| **Price Band Protection** | Reject orders outside X% from last trade / mid-price |
| **Max order size** | Reject orders exceeding pair max |
| **Min order size** | Reject orders below pair min |
| **Tick/lot validation** | Reject orders not aligned to tick/lot size |

### Market Data (real-time)
| Feed | Description |
|------|-------------|
| **Order book depth** | Aggregated bids/asks by price level |
| **Best Bid/Offer (BBO)** | Top of book |
| **Recent trades** | Trade feed with price, qty, timestamp |
| **Ticker** | Last price, 24h high/low/volume/change |

### Audit Trail
- Immutable append-only log of every event:
  - Order created, modified, canceled
  - Match executed (both sides)
  - Trade confirmed
- Each event: sequence number, timestamp, pair_id, order details
- Enables deterministic replay

### Sequence Numbering
- Global sequence per pair (monotonically increasing)
- Every event gets a sequence number
- Enables gap detection and replay

---

## Tier 2 — Standard (post-PoC)

### Order Types
| Type | Description |
|------|-------------|
| **Stop-Market** | Triggers market order when stop price reached |
| **Stop-Limit** | Triggers limit order when stop price reached |

### Time-in-Force
| TIF | Description |
|-----|-------------|
| **GTD** | Good Til Date — expires at specified time |

### Safety
| Feature | Description |
|---------|-------------|
| **Circuit Breaker** | Auto-halt pair if price moves X% in Y seconds |
| **Rate Limiting** | Max orders/sec per user/account |
| **Kill Switch** | Admin: halt all trading instantly |
| **Order throttling** | Slow down processing under extreme load |

### Resilience
| Feature | Description |
|---------|-------------|
| **Idempotent processing** | Duplicate messages produce no double-effects |
| **Graceful shutdown** | Drain in-flight orders before stopping |
| **Dead letter queue** | Failed messages routed for inspection |
| **Deterministic replay** | Rebuild state from audit log |

---

## Tier 3 — Advanced (production)

### Order Types
| Type | Description |
|------|-------------|
| **Trailing Stop** | Stop price follows market by offset |
| **OCO** | One-Cancels-the-Other (pair of orders) |
| **Iceberg** | Large order shown in small visible slices |
| **Peg** | Price pegged to BBO / mid-price |

### Matching
| Feature | Description |
|---------|-------------|
| **Pro-Rata** | Allocate fills proportional to order size (optional mode) |
| **Auction mode** | Opening/closing auction with indicative price |

### Infrastructure
| Feature | Description |
|---------|-------------|
| **FIX protocol** | Industry-standard trading protocol gateway |
| **Multi-tenancy** | Isolated contexts per exchange/venue |
| **Hot standby** | Instant failover to replica |
