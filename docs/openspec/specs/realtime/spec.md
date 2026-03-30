# Real-Time Feeds Specification

## Purpose
WebSocket and Server-Sent Events (SSE) feeds for pushing live market data and
order updates to clients. The gateway uses CacheBroadcasts (in-memory watch +
broadcast channels) to fan out to N clients from a single Valkey poller.

---

## Requirements

### Requirement: WebSocket Order Book Feed
The system SHALL provide a per-pair WebSocket feed for order book updates.

#### Scenario: Client receives current book on connect
- GIVEN a client connects to ws://.../ws/orderbook/BTC-USDT
- WHEN the connection is established
- THEN the current order book state is immediately pushed (no wait for next update)

#### Scenario: Client receives updates on book change
- GIVEN a client subscribed to ws://.../ws/orderbook/BTC-USDT
- WHEN a new order is placed or matched
- THEN an updated order book is pushed within 100ms

#### Scenario: Client disconnect is handled cleanly
- GIVEN a connected WS client
- WHEN the client disconnects
- THEN the server cleans up the subscription without error

---

### Requirement: WebSocket Trades Feed
The system SHALL provide a per-pair WebSocket feed for trade events.

#### Scenario: Trade pushed on match
- GIVEN a client subscribed to ws://.../ws/trades/BTC-USDT
- WHEN a match occurs
- THEN the trade event is pushed to the client

---

### Requirement: WebSocket Order Updates Feed
The system SHALL provide a per-user WebSocket feed for order status changes.

#### Scenario: Order fill event pushed to owner
- GIVEN user-1 connected to ws://.../ws/orders/user-1
- WHEN user-1's order is filled
- THEN the fill event is pushed to that client only

#### Scenario: Other users' events not delivered
- GIVEN user-1 connected to ws://.../ws/orders/user-1
- WHEN user-2's order is filled
- THEN no message is pushed to user-1's connection

---

### Requirement: Multiplexed WebSocket Stream
The system SHALL provide a single multiplexed WebSocket endpoint at /ws/stream
that supports multiple subscriptions over one connection.

Subscription protocol:
- Client sends: `{"subscribe": ["orderbook:BTC-USDT", "trades:BTC-USDT", "ticker:BTC-USDT"]}`
- Server pushes: `{"ch": "<channel>", "data": <payload>}`

#### Scenario: Subscribe to multiple channels
- GIVEN a client connects to /ws/stream
- WHEN the client sends a subscribe message with multiple channels
- THEN the server begins pushing updates for each subscribed channel

#### Scenario: Initial snapshot on subscribe
- GIVEN a client subscribes to orderbook:BTC-USDT
- WHEN the subscription is accepted
- THEN the current cached value is immediately pushed before any updates

#### Scenario: User-specific channels over mux WS
- GIVEN a client subscribes to orders:user-1 and portfolio:user-1
- WHEN order or portfolio events occur for user-1
- THEN those events are pushed to this connection only

---

### Requirement: Server-Sent Events (SSE) Stream
The system SHALL provide an SSE endpoint at /api/stream/{pair_id} as an
alternative to WebSocket for clients where WS is not available.

#### Scenario: SSE initial snapshot
- GIVEN a client connects to GET /api/stream/BTC-USDT
- WHEN the connection is established
- THEN the client receives named events (orderbook, trades, ticker, metrics, throughput, latency)
  with current values immediately

#### Scenario: SSE updates flow continuously
- GIVEN an active SSE connection
- WHEN market data changes
- THEN the corresponding named event is pushed

#### Scenario: SSE keep-alive
- GIVEN an idle SSE connection with no data changes
- WHEN 15 seconds pass
- THEN a keep-alive comment is sent to prevent connection timeout

---

### Requirement: WS Broadcast Rate Limiting
The system SHALL throttle WebSocket fan-out to a maximum of 10 broadcasts/second
per cache key to prevent overwhelming clients.

#### Scenario: High-frequency updates are throttled
- GIVEN 200 order updates per second arriving
- WHEN the WS broadcast fires
- THEN clients receive at most 10 messages/second per subscription channel

---

### Requirement: WS Ping/Pong
The system SHALL respond to WebSocket Ping frames with Pong frames.

#### Scenario: Ping acknowledged
- GIVEN an active WS connection
- WHEN the client sends a Ping frame
- THEN the server responds with a Pong frame
