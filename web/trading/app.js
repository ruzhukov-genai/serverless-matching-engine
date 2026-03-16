// SME Trading UI — fully functional client
'use strict';

const API = window.location.origin;
let currentPair = null;
let currentPairConfig = null;
let currentSide = 'buy';
let orderbook = { bids: [], asks: [] };
let tradesList = [];
let ws = { orderbook: null, trades: null, stream: null };
let fallbackPolls = {};
let feedbackTimer = null;
// Set true to use single multiplexed WS (fewer TCP connections, lower latency)
const USE_MUX_WS = true;

// Pagination state
let ordersPage = 0;
const ORDERS_PER_PAGE = 20;
let ordersTotal = 0;

// ── Init ──────────────────────────────────────────────────────────────────────

async function init() {
    setupTabs();
    setupOrderForm();
    await loadPairs();
    startAutoRefresh();
}

// ── Pairs ─────────────────────────────────────────────────────────────────────

async function loadPairs() {
    try {
        const res = await fetch(`${API}/api/pairs`);
        const data = await res.json();
        const pairs = data.pairs || [];
        const tradingPairs = pairs.filter(p => !p.id.startsWith('BAL-'));
        renderPairs(tradingPairs);
        if (tradingPairs.length > 0) selectPair(tradingPairs[0]);
    } catch (e) {
        console.error('Failed to load pairs:', e);
        showFeedback('Failed to load trading pairs', 'error');
    }
}

function renderPairs(pairs) {
    const el = document.getElementById('pairs-list');
    el.innerHTML = pairs.map(p =>
        `<button data-pair="${p.id}" onclick="selectPairById('${p.id}')">${p.id}</button>`
    ).join('');
    el._pairs = pairs;
}

window.selectPairById = function(pairId) {
    const el = document.getElementById('pairs-list');
    const p = (el._pairs || []).find(x => x.id === pairId);
    if (p) selectPair(p);
};

function selectPair(pair) {
    currentPair = pair.id;
    currentPairConfig = pair;

    document.getElementById('current-pair').textContent = pair.id;
    document.querySelectorAll('.pairs-bar button').forEach(b =>
        b.classList.toggle('active', b.dataset.pair === pair.id)
    );

    orderbook = { bids: [], asks: [] };
    tradesList = [];
    ordersPage = 0;

    clearFallbackPoll('orderbook');
    clearFallbackPoll('trades');

    loadOrderbook(pair.id);
    loadTrades(pair.id);
    connectWebSockets(pair.id);
    loadOpenOrders();
}

// ── Order Book ────────────────────────────────────────────────────────────────

async function loadOrderbook(pairId) {
    try {
        const res = await fetch(`${API}/api/orderbook/${pairId}`);
        const data = await res.json();
        orderbook.bids = data.bids || [];
        orderbook.asks = data.asks || [];
        renderOrderbook();
    } catch (e) {
        console.error('Failed to load orderbook:', e);
    }
}

function renderOrderbook() {
    const asksEl = document.getElementById('asks');
    const bidsEl = document.getElementById('bids');
    const spreadEl = document.getElementById('spread-value');
    const lastPriceEl = document.getElementById('last-price');

    // Sort asks low→high (best ask at bottom, near spread)
    const sortedAsks = [...orderbook.asks]
        .sort((a, b) => parseFloat(a.price) - parseFloat(b.price))
        .slice(0, 25);

    // Sort bids high→low (best bid at top, near spread)
    const sortedBids = [...orderbook.bids]
        .sort((a, b) => parseFloat(b.price) - parseFloat(a.price))
        .slice(0, 25);

    // Compute cumulative totals
    const asksCum = computeCumulative(sortedAsks);
    const bidsCum = computeCumulative(sortedBids);

    // Max cumulative for depth bar sizing
    const maxCum = Math.max(
        asksCum.length > 0 ? asksCum[asksCum.length - 1].cum : 0,
        bidsCum.length > 0 ? bidsCum[bidsCum.length - 1].cum : 0,
        1
    );

    // Asks: reverse so highest price is at top, lowest (best) near spread
    const asksReversed = [...asksCum].reverse();

    asksEl.innerHTML = asksReversed.map(l =>
        renderLevel(l, 'ask', maxCum)
    ).join('');

    bidsEl.innerHTML = bidsCum.map(l =>
        renderLevel(l, 'bid', maxCum)
    ).join('');

    // Spread
    if (sortedAsks.length > 0 && sortedBids.length > 0) {
        const bestAsk = parseFloat(sortedAsks[0].price);
        const bestBid = parseFloat(sortedBids[0].price);
        const spread = bestAsk - bestBid;
        const pct = bestBid > 0 ? ((spread / bestBid) * 100).toFixed(2) : '0.00';
        spreadEl.textContent = `Spread: ${fmtPrice(spread)} (${pct}%)`;

        // Last price = midpoint for display
        if (lastPriceEl) {
            lastPriceEl.textContent = fmtPrice((bestAsk + bestBid) / 2);
        }
    } else {
        spreadEl.textContent = '—';
    }

    // Auto-scroll asks to bottom (near spread)
    asksEl.scrollTop = asksEl.scrollHeight;
}

function computeCumulative(levels) {
    let cum = 0;
    return levels.map(l => {
        const qty = parseFloat(l.quantity) || 0;
        cum += qty;
        return { ...l, cum };
    });
}

function renderLevel(level, side, maxCum) {
    const price = parseFloat(level.price);
    const qty   = parseFloat(level.quantity);
    const cum   = level.cum || 0;
    const depthPct = Math.min(100, (cum / maxCum) * 100).toFixed(1);
    const color = side === 'ask' ? 'rgba(248,81,73,0.25)' : 'rgba(63,185,80,0.25)';
    const bg = `linear-gradient(to left, ${color} ${depthPct}%, transparent ${depthPct}%)`;

    return `<div class="level ${side}" onclick="fillPrice(${price})" style="background:${bg}">` +
        `<span class="level-price">${fmtPrice(price)}</span>` +
        `<span class="level-qty">${fmtQty(qty)}</span>` +
        `<span class="level-cum">${fmtQty(cum)}</span>` +
        `</div>`;
}

window.fillPrice = function(price) {
    const priceInput = document.getElementById('order-price');
    priceInput.value = price;
    priceInput.dispatchEvent(new Event('input'));
};

// ── Trades ────────────────────────────────────────────────────────────────────

async function loadTrades(pairId) {
    try {
        const res = await fetch(`${API}/api/trades/${pairId}`);
        const data = await res.json();
        tradesList = data.trades || [];
        renderTrades();
    } catch (e) {
        console.error('Failed to load trades:', e);
    }
}

function renderTrades() {
    const el = document.getElementById('trades-list');
    if (tradesList.length === 0) {
        el.innerHTML = '<div class="empty-state">No recent trades</div>';
        return;
    }
    el.innerHTML = tradesList.slice(0, 50).map(t => {
        const side = (t.side || 'buy').toLowerCase();
        return `<div class="trade-row ${side}">` +
            `<span class="trade-price">${fmtPrice(parseFloat(t.price))}</span>` +
            `<span class="trade-qty">${fmtQty(parseFloat(t.quantity))}</span>` +
            `<span class="trade-time">${fmtTime(t.created_at)}</span>` +
            `</div>`;
    }).join('');
}

function prependTrade(trade) {
    tradesList.unshift(trade);
    if (tradesList.length > 200) tradesList.length = 200;
    renderTrades();
}

// ── WebSocket ─────────────────────────────────────────────────────────────────

function connectWebSockets(pairId) {
    const wsBase = API.replace(/^https?/, proto => proto === 'https' ? 'wss' : 'ws');
    if (USE_MUX_WS) {
        connectMuxWS(wsBase, pairId);
    } else {
        connectOrderbookWS(wsBase, pairId);
        connectTradesWS(wsBase, pairId);
        connectOrdersWS(wsBase);
    }
}

// ── Multiplexed WS — single connection for all data ──────────────────────────
// Replaces 3 separate WS + REST polling with one connection.
// Subscribes to: orderbook, trades, ticker, orders, portfolio for current pair.

function connectMuxWS(wsBase, pairId) {
    // Close previous mux connection
    if (ws.stream) {
        ws.stream.onclose = null;
        try { ws.stream.close(); } catch(_) {}
    }

    const userId = 'user-1'; // TODO: make configurable
    try {
        const sock = new WebSocket(`${wsBase}/ws/stream`);
        ws.stream = sock;
        sock.onopen = () => {
            // Subscribe to all channels for this pair + user
            sock.send(JSON.stringify({ subscribe: [
                `orderbook:${pairId}`,
                `trades:${pairId}`,
                `ticker:${pairId}`,
                `orders:${userId}`,
                `portfolio:${userId}`,
            ]}));
            clearFallbackPoll('orderbook');
            clearFallbackPoll('trades');
            clearFallbackPoll('orders');
            clearFallbackPoll('portfolio');
        };
        sock.onmessage = e => {
            try {
                const envelope = JSON.parse(e.data);
                const ch = envelope.ch;
                const data = envelope.data;
                if (!ch || data === undefined) return;

                if (ch.startsWith('orderbook:')) {
                    if (data.bids || data.asks) {
                        orderbook.bids = data.bids || orderbook.bids;
                        orderbook.asks = data.asks || orderbook.asks;
                        renderOrderbook();
                    }
                } else if (ch.startsWith('trades:')) {
                    if (data.trades && Array.isArray(data.trades)) {
                        tradesList = data.trades;
                        renderTrades();
                    } else if (data.price != null) {
                        prependTrade(data);
                    }
                } else if (ch.startsWith('ticker:')) {
                    // Update ticker display if we have one
                    updateTicker(data);
                } else if (ch.startsWith('orders:')) {
                    // Order event — refresh order list
                    if (data.type === 'order_created' || data.type === 'order_cancelled' || data.type === 'orders_cancelled_all') {
                        loadOpenOrders();
                    }
                } else if (ch.startsWith('portfolio:')) {
                    // Portfolio pushed from server
                    if (data.balances) {
                        renderPortfolio(data.balances);
                    }
                }
            } catch(err) { console.warn('Mux WS parse error:', err); }
        };
        sock.onerror = () => {};
        sock.onclose = () => {
            // Fall back to REST polling if WS drops
            if (currentPair === pairId) {
                startFallbackPoll('orderbook', () => loadOrderbook(pairId), 2500);
                startFallbackPoll('trades', () => loadTrades(pairId), 2500);
                startFallbackPoll('orders', () => loadOpenOrders(), 5000);
                startFallbackPoll('portfolio', () => loadPortfolio(), 5000);
                // Reconnect after delay
                setTimeout(() => {
                    if (currentPair === pairId) connectMuxWS(wsBase, pairId);
                }, 2000);
            }
        };
    } catch(e) {
        startFallbackPoll('orderbook', () => loadOrderbook(pairId), 2500);
        startFallbackPoll('trades', () => loadTrades(pairId), 2500);
    }
}

function updateTicker(data) {
    // Update the last price / ticker info in the UI
    const lastPriceEl = document.getElementById('last-price');
    if (lastPriceEl && data.last) {
        lastPriceEl.textContent = fmtPrice(parseFloat(data.last));
    }
}

// ── Legacy WS connections (used when USE_MUX_WS = false) ─────────────────────

function connectOrderbookWS(wsBase, pairId) {
    if (ws.orderbook) {
        ws.orderbook.onclose = null;
        try { ws.orderbook.close(); } catch(_) {}
    }
    try {
        const sock = new WebSocket(`${wsBase}/ws/orderbook/${pairId}`);
        ws.orderbook = sock;
        sock.onopen = () => clearFallbackPoll('orderbook');
        sock.onmessage = e => {
            try {
                const msg = JSON.parse(e.data);
                if (msg.type === 'snapshot') {
                    orderbook.bids = msg.bids || [];
                    orderbook.asks = msg.asks || [];
                } else if (msg.type === 'update') {
                    applyBookUpdate(msg);
                } else if (msg.bids || msg.asks) {
                    orderbook.bids = msg.bids || orderbook.bids;
                    orderbook.asks = msg.asks || orderbook.asks;
                }
                renderOrderbook();
            } catch(err) { console.warn('OB WS parse error:', err); }
        };
        sock.onerror = () => {};
        sock.onclose = () => {
            if (currentPair === pairId)
                startFallbackPoll('orderbook', () => loadOrderbook(pairId), 2500);
        };
    } catch(e) {
        startFallbackPoll('orderbook', () => loadOrderbook(pairId), 2500);
    }
}

function connectTradesWS(wsBase, pairId) {
    if (ws.trades) {
        ws.trades.onclose = null;
        try { ws.trades.close(); } catch(_) {}
    }
    try {
        const sock = new WebSocket(`${wsBase}/ws/trades/${pairId}`);
        ws.trades = sock;
        sock.onopen = () => clearFallbackPoll('trades');
        sock.onmessage = e => {
            try {
                const msg = JSON.parse(e.data);
                if (Array.isArray(msg.trades)) {
                    msg.trades.forEach(t => prependTrade(t));
                } else if (msg.price != null) {
                    prependTrade(msg);
                }
            } catch(err) { console.warn('Trades WS parse error:', err); }
        };
        sock.onerror = () => {};
        sock.onclose = () => {
            if (currentPair === pairId)
                startFallbackPoll('trades', () => loadTrades(pairId), 2500);
        };
    } catch(e) {
        startFallbackPoll('trades', () => loadTrades(pairId), 2500);
    }
}

function connectOrdersWS(wsBase) {
    if (ws.orders) {
        ws.orders.onclose = null;
        try { ws.orders.close(); } catch(_) {}
    }
    try {
        const userId = 'user-1'; // TODO: make configurable
        const sock = new WebSocket(`${wsBase}/ws/orders/${userId}`);
        ws.orders = sock;
        sock.onopen = () => clearFallbackPoll('orders');
        sock.onmessage = e => {
            try {
                const msg = JSON.parse(e.data);
                if (msg.type === 'snapshot') {
                    ordersTotal = msg.total || 0;
                    renderOpenOrders(msg.orders || []);
                } else if (msg.type === 'order_created' || msg.type === 'order_cancelled' || msg.type === 'orders_cancelled_all') {
                    loadOpenOrders();
                    loadPortfolio();
                }
            } catch(err) { console.warn('Orders WS parse error:', err); }
        };
        sock.onerror = () => {};
        sock.onclose = () => {
            startFallbackPoll('orders', () => loadOpenOrders(), 5000);
        };
    } catch(e) {
        startFallbackPoll('orders', () => loadOpenOrders(), 5000);
    }
}

function applyBookUpdate(msg) {
    if (msg.bids) msg.bids.forEach(l => applyLevel(orderbook.bids, l));
    if (msg.asks) msg.asks.forEach(l => applyLevel(orderbook.asks, l));
}

function applyLevel(arr, level) {
    const price = parseFloat(level.price);
    const qty   = parseFloat(level.quantity);
    const idx   = arr.findIndex(x => parseFloat(x.price) === price);
    if (qty === 0) {
        if (idx >= 0) arr.splice(idx, 1);
    } else {
        if (idx >= 0) arr[idx] = level;
        else arr.push(level);
    }
}

function startFallbackPoll(key, fn, interval) {
    if (fallbackPolls[key]) return;
    fn();
    fallbackPolls[key] = setInterval(fn, interval);
}

function clearFallbackPoll(key) {
    if (fallbackPolls[key]) {
        clearInterval(fallbackPolls[key]);
        delete fallbackPolls[key];
    }
}

// ── Order Entry ───────────────────────────────────────────────────────────────

function setupTabs() {
    document.querySelectorAll('.tab').forEach(tab => {
        tab.addEventListener('click', () => {
            currentSide = tab.dataset.side;
            document.querySelectorAll('.tab').forEach(t => t.classList.remove('active'));
            tab.classList.add('active');
            const btn = document.getElementById('submit-order');
            btn.textContent = currentSide === 'buy' ? 'Buy' : 'Sell';
            btn.className   = currentSide === 'buy' ? 'btn-buy' : 'btn-sell';
        });
    });
}

function setupOrderForm() {
    const typeSelect = document.getElementById('order-type');
    const priceGroup = document.getElementById('price-group');
    const priceInput = document.getElementById('order-price');
    const qtyInput   = document.getElementById('order-quantity');
    const totalEl    = document.getElementById('order-total');

    typeSelect.addEventListener('change', () => {
        priceGroup.style.display = typeSelect.value === 'market' ? 'none' : '';
        updateTotal();
    });

    function updateTotal() {
        const p = parseFloat(priceInput.value) || 0;
        const q = parseFloat(qtyInput.value) || 0;
        totalEl.textContent = (p * q).toFixed(2);
    }

    priceInput.addEventListener('input', updateTotal);
    qtyInput.addEventListener('input', updateTotal);

    document.getElementById('order-form').addEventListener('submit', async e => {
        e.preventDefault();
        await submitOrder();
    });
}

async function submitOrder() {
    if (!currentPair) {
        showFeedback('Select a trading pair first', 'error');
        return;
    }

    const orderType = document.getElementById('order-type').value;
    const price     = document.getElementById('order-price').value;
    const quantity  = document.getElementById('order-quantity').value;
    const tif       = document.getElementById('order-tif').value;

    if (!quantity || parseFloat(quantity) <= 0) {
        showFeedback('Enter a valid quantity', 'error');
        return;
    }
    if (orderType === 'limit' && (!price || parseFloat(price) <= 0)) {
        showFeedback('Enter a valid price', 'error');
        return;
    }

    const sideCapital = currentSide.charAt(0).toUpperCase() + currentSide.slice(1);
    const typeCapital = orderType.charAt(0).toUpperCase() + orderType.slice(1);

    const body = {
        pair_id:    currentPair,
        side:       sideCapital,
        order_type: typeCapital,
        quantity:   quantity,
        tif:        tif.toUpperCase(),
    };
    if (orderType === 'limit') body.price = price;

    const btn = document.getElementById('submit-order');
    btn.disabled    = true;
    btn.textContent = 'Submitting…';

    try {
        const res = await fetch(`${API}/api/orders`, {
            method:  'POST',
            headers: { 'Content-Type': 'application/json' },
            body:    JSON.stringify(body),
        });

        if (res.ok) {
            const data = await res.json();
            const trades = data.trades || [];
            showFeedback(
                trades.length > 0
                    ? `${sideCapital} filled: ${trades.length} trade(s)!`
                    : `${sideCapital} order placed`,
                'success'
            );
            document.getElementById('order-quantity').value = '';
            document.getElementById('order-total').textContent = '0.00';
            // Refresh everything
            await Promise.all([
                loadPortfolio(),
                loadOpenOrders(),
                loadOrderbook(currentPair),
                loadTrades(currentPair),
            ]);
        } else {
            const err = await res.json().catch(() => ({}));
            showFeedback(`Order failed: ${err.message || err.error || res.statusText}`, 'error');
        }
    } catch (e) {
        showFeedback(`Order failed: ${e.message}`, 'error');
    } finally {
        btn.disabled    = false;
        btn.textContent = currentSide === 'buy' ? 'Buy' : 'Sell';
    }
}

// ── Portfolio ─────────────────────────────────────────────────────────────────

async function loadPortfolio() {
    try {
        const res = await fetch(`${API}/api/portfolio`);
        const data = await res.json();
        renderPortfolio(data.balances || []);
    } catch (e) {
        console.error('Failed to load portfolio:', e);
    }
}

function renderPortfolio(balances) {
    const tbody = document.getElementById('portfolio-body');
    if (balances.length === 0) {
        tbody.innerHTML = '<tr><td colspan="4" class="empty">No balances</td></tr>';
        return;
    }
    tbody.innerHTML = balances.map(b => {
        const avail = parseFloat(b.available) || 0;
        const locked = parseFloat(b.locked) || 0;
        return `<tr>` +
            `<td class="asset-name">${esc(b.asset)}</td>` +
            `<td>${fmtQty(avail)}</td>` +
            `<td>${locked > 0 ? fmtQty(locked) : '<span class="text-muted">—</span>'}</td>` +
            `<td>${fmtQty(avail + locked)}</td>` +
            `</tr>`;
    }).join('');
}

// ── Open Orders ───────────────────────────────────────────────────────────────

async function loadOpenOrders() {
    try {
        const offset = ordersPage * ORDERS_PER_PAGE;
        const res = await fetch(`${API}/api/orders?limit=${ORDERS_PER_PAGE}&offset=${offset}`);
        const data = await res.json();
        ordersTotal = data.total || 0;
        renderOpenOrders(data.orders || []);
    } catch (e) {
        console.error('Failed to load orders:', e);
    }
}

function renderOpenOrders(orders) {
    const tbody = document.getElementById('orders-body');

    if (orders.length === 0 && ordersTotal === 0) {
        tbody.innerHTML = '<tr><td colspan="7" class="empty">No open orders</td></tr>';
        renderOrdersPagination(0);
        return;
    }

    tbody.innerHTML = orders.map(o => {
        const qty    = parseFloat(o.quantity) || 0;
        const rem    = parseFloat(o.remaining != null ? o.remaining : qty);
        const filled = qty - rem;
        const side   = (o.side || 'buy').toLowerCase();
        const status = (o.status || 'new').toLowerCase();
        return `<tr>` +
            `<td>${esc(o.pair_id)}</td>` +
            `<td class="side-${side}">${side.toUpperCase()}</td>` +
            `<td>${o.price != null ? fmtPrice(parseFloat(o.price)) : '<span class="text-muted">MKT</span>'}</td>` +
            `<td>${fmtQty(qty)}</td>` +
            `<td>${filled > 0 ? fmtQty(filled) : '<span class="text-muted">—</span>'}</td>` +
            `<td>${fmtTime(o.created_at)}</td>` +
            `<td><button class="cancel-btn" onclick="cancelOrder('${esc(o.id)}')">✕</button></td>` +
            `</tr>`;
    }).join('');

    renderOrdersPagination(Math.ceil(ordersTotal / ORDERS_PER_PAGE));
}

function renderOrdersPagination(totalPages) {
    const container = document.getElementById('orders-pagination');
    if (!container) return;

    if (ordersTotal <= ORDERS_PER_PAGE) {
        container.innerHTML = ordersTotal > 0
            ? `<span class="page-info">${ordersTotal} order${ordersTotal === 1 ? '' : 's'}</span>`
            : '';
        return;
    }

    const start = ordersPage * ORDERS_PER_PAGE + 1;
    const end = Math.min((ordersPage + 1) * ORDERS_PER_PAGE, ordersTotal);

    container.innerHTML =
        `<button class="page-btn" onclick="ordersPageNav(-1)" ${ordersPage === 0 ? 'disabled' : ''}>‹</button>` +
        `<span class="page-info">${start}–${end} of ${ordersTotal.toLocaleString()}</span>` +
        `<button class="page-btn" onclick="ordersPageNav(1)" ${ordersPage >= totalPages - 1 ? 'disabled' : ''}>›</button>`;
}

window.ordersPageNav = function(delta) {
    const totalPages = Math.ceil(ordersTotal / ORDERS_PER_PAGE);
    ordersPage = Math.max(0, Math.min(totalPages - 1, ordersPage + delta));
    loadOpenOrders();
};

window.cancelOrder = async function(orderId) {
    try {
        const res = await fetch(`${API}/api/orders/${orderId}`, { method: 'DELETE' });
        if (res.ok) {
            showFeedback('Order cancelled', 'success');
            await Promise.all([loadOpenOrders(), loadPortfolio(), loadOrderbook(currentPair)]);
        } else {
            showFeedback('Failed to cancel order', 'error');
        }
    } catch (e) {
        showFeedback(`Cancel failed: ${e.message}`, 'error');
    }
};

window.cancelAllOrders = async function() {
    if (!confirm(`Cancel all ${ordersTotal.toLocaleString()} open orders?`)) return;
    try {
        const res = await fetch(`${API}/api/orders`, { method: 'DELETE' });
        if (res.ok) {
            const data = await res.json();
            showFeedback(`Cancelled ${data.cancelled.toLocaleString()} orders`, 'success');
            ordersPage = 0;
            await Promise.all([loadOpenOrders(), loadPortfolio(), loadOrderbook(currentPair)]);
        } else {
            showFeedback('Failed to cancel orders', 'error');
        }
    } catch (e) {
        showFeedback(`Cancel failed: ${e.message}`, 'error');
    }
};

// ── Auto Refresh ──────────────────────────────────────────────────────────────

function startAutoRefresh() {
    loadPortfolio();
    if (!USE_MUX_WS) {
        // Legacy mode: poll portfolio via REST (orders pushed via WS)
        setInterval(() => {
            loadPortfolio();
        }, 5000);
    }
    // In mux WS mode, portfolio is pushed over the stream — no polling needed
}

// ── Feedback Toast ────────────────────────────────────────────────────────────

function showFeedback(message, type = 'info') {
    let el = document.getElementById('feedback-toast');
    if (!el) {
        el = document.createElement('div');
        el.id = 'feedback-toast';
        document.body.appendChild(el);
    }
    el.textContent = message;
    el.className = `feedback-toast feedback-${type} show`;
    if (feedbackTimer) clearTimeout(feedbackTimer);
    feedbackTimer = setTimeout(() => el.classList.remove('show'), 4000);
}

// ── Formatters ────────────────────────────────────────────────────────────────

function fmtPrice(n) {
    if (n == null || isNaN(n)) return '—';
    if (n >= 1000) return n.toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 });
    if (n >= 1)    return n.toFixed(4);
    return n.toFixed(6);
}

function fmtQty(n) {
    if (n == null || isNaN(n)) return '—';
    if (n >= 1000) return n.toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 });
    if (n >= 1)    return n.toFixed(4);
    return n.toFixed(5);
}

function fmtTime(ts) {
    if (!ts) return '—';
    const d = new Date(ts);
    if (isNaN(d.getTime())) return '—';
    return d.toTimeString().slice(0, 8);
}

function esc(s) {
    if (s == null) return '';
    return String(s)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;');
}

// ── Boot ──────────────────────────────────────────────────────────────────────

init();
