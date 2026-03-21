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

// ── P0 — requestAnimationFrame batching ─────────────────────────────────────
let obRafId = 0;   // separate rAF IDs so orderbook + trades don't clobber each other
let trRafId = 0;
let tradesChanged = false;

// DOM element caches for diff-based updates
let asksDomMap = new Map(); // price -> DOM element
let bidsDomMap = new Map(); // price -> DOM element

// ── P3 — Cached Intl.NumberFormat instead of toLocaleString ────────────────
const fmtPrice2 = new Intl.NumberFormat('en-US', {minimumFractionDigits:2, maximumFractionDigits:2});
const fmtPrice4 = new Intl.NumberFormat('en-US', {minimumFractionDigits:4, maximumFractionDigits:4});
const fmtPrice6 = new Intl.NumberFormat('en-US', {minimumFractionDigits:6, maximumFractionDigits:6});
const fmtQty2 = new Intl.NumberFormat('en-US', {minimumFractionDigits:2, maximumFractionDigits:2});
const fmtQty4 = new Intl.NumberFormat('en-US', {minimumFractionDigits:4, maximumFractionDigits:4});
const fmtQty5 = new Intl.NumberFormat('en-US', {minimumFractionDigits:5, maximumFractionDigits:5});

// ── P2 — Connection status and exponential backoff ─────────────────────────
let connStatus = 'disconnected'; // 'connected', 'fallback', 'disconnected'
let reconnectDelay = 1000; // Start at 1s
const MAX_RECONNECT_DELAY = 30000; // Max 30s
let reconnectTimeout = null;

// Pagination state
let ordersPage = 0;
const ORDERS_PER_PAGE = 20;
let ordersTotal = 0;

// ── Init ──────────────────────────────────────────────────────────────────────

async function init() {
    setupTabs();
    setupOrderForm();
    updateConnectionStatus('disconnected');
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

    // ── P2 — Show loading states instead of clearing data ──────────────────
    document.getElementById('orderbook').classList.add('loading');
    document.getElementById('trades').classList.add('loading');
    
    // Clear DOM maps for new pair
    asksDomMap.clear();
    bidsDomMap.clear();
    lastTradesListLength = 0;
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
        // P1 — Request with default depth limit of 25
        const res = await fetch(`${API}/api/orderbook/${pairId}?depth=25`);
        const data = await res.json();
        orderbook.bids = data.bids || [];
        orderbook.asks = data.asks || [];
        renderOrderbook();
    } catch (e) {
        console.error('Failed to load orderbook:', e);
        document.getElementById('orderbook').classList.remove('loading');
    }
}

function renderOrderbook() {
    // ── P0 — Coalesce into one rAF per frame; cancel stale frame if data arrives again
    if (obRafId) cancelAnimationFrame(obRafId);
    obRafId = requestAnimationFrame(doRenderOrderbook);
}

function doRenderOrderbook() {
    obRafId = 0;
    
    // Remove loading state
    document.getElementById('orderbook').classList.remove('loading');
    
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

    // ── P0 — Diff-based updates for asks ──────────────────────────────────
    updateOrderbookSide(asksEl, asksReversed, 'ask', maxCum, asksDomMap);
    updateOrderbookSide(bidsEl, bidsCum, 'bid', maxCum, bidsDomMap);

    // Spread calculation
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

function updateOrderbookSide(container, levels, side, maxCum, domMap) {
    // Create a set of current prices for quick lookup
    const currentPrices = new Set(levels.map(l => parseFloat(l.price)));
    
    // Remove elements for prices no longer in the book
    for (const [price, element] of domMap) {
        if (!currentPrices.has(price)) {
            element.remove();
            domMap.delete(price);
        }
    }
    
    // Update or create elements for each level
    const fragment = document.createDocumentFragment();
    let needsReorder = false;
    
    levels.forEach((level, index) => {
        const price = parseFloat(level.price);
        let element = domMap.get(price);
        
        if (!element) {
            // Create new element
            element = document.createElement('div');
            element.className = `level ${side}`;
            element.onclick = () => fillPrice(price);
            domMap.set(price, element);
            needsReorder = true;
        }
        
        // Update element content
        updateLevelElement(element, level, side, maxCum);
        
        // Add to fragment if it's a new element
        if (!element.parentNode) {
            fragment.appendChild(element);
        }
    });
    
    // Add any new elements
    if (fragment.children.length > 0) {
        container.appendChild(fragment);
        needsReorder = true;
    }
    
    // Reorder if needed (only when elements were added/removed)
    if (needsReorder) {
        const sortedElements = levels.map(l => domMap.get(parseFloat(l.price)));
        sortedElements.forEach(element => {
            if (element) container.appendChild(element);
        });
    }
}

function updateLevelElement(element, level, side, maxCum) {
    const price = parseFloat(level.price);
    const qty = parseFloat(level.quantity);
    const cum = level.cum || 0;
    const depthPct = Math.min(100, (cum / maxCum) * 100).toFixed(1);
    const color = side === 'ask' ? 'rgba(248,81,73,0.25)' : 'rgba(63,185,80,0.25)';

    element.style.background = `linear-gradient(to left, ${color} ${depthPct}%, transparent ${depthPct}%)`;

    // Update text content of existing spans (avoids innerHTML reparse)
    const spans = element.children;
    if (spans.length === 3) {
        const newPrice = fmtPrice(price);
        const newQty   = fmtQty(qty);
        const newCum   = fmtQty(cum);
        if (spans[0].textContent !== newPrice) spans[0].textContent = newPrice;
        if (spans[1].textContent !== newQty)   spans[1].textContent = newQty;
        if (spans[2].textContent !== newCum)   spans[2].textContent = newCum;
    } else {
        // First render — create spans
        element.innerHTML =
            `<span class="level-price">${fmtPrice(price)}</span>` +
            `<span class="level-qty">${fmtQty(qty)}</span>` +
            `<span class="level-cum">${fmtQty(cum)}</span>`;
    }
}

function computeCumulative(levels) {
    let cum = 0;
    return levels.map(l => {
        const qty = parseFloat(l.quantity) || 0;
        cum += qty;
        return { ...l, cum };
    });
}

// Legacy renderLevel function - kept for compatibility but not used in diff updates
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
        document.getElementById('trades').classList.remove('loading');
    }
}

function renderTrades() {
    // ── P0 — Batch trades renders; skip if nothing changed ────────────────
    tradesChanged = true;
    if (trRafId) cancelAnimationFrame(trRafId);
    trRafId = requestAnimationFrame(doRenderTrades);
}

function doRenderTrades() {
    trRafId = 0;
    if (!tradesChanged) return;
    tradesChanged = false;
    
    // Remove loading state
    document.getElementById('trades').classList.remove('loading');
    
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
    
    // Clear any pending reconnect
    if (reconnectTimeout) {
        clearTimeout(reconnectTimeout);
        reconnectTimeout = null;
    }

    const userId = 'user-1'; // TODO: make configurable
    try {
        const sock = new WebSocket(`${wsBase}/ws/stream`);
        ws.stream = sock;
        sock.onopen = () => {
            // ── P2 — Update connection status ─────────────────────────────────
            updateConnectionStatus('connected');
            // Reset backoff on successful connection
            reconnectDelay = 1000;
            
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
                    updateTicker(data);
                } else if (ch.startsWith('orders:')) {
                    // ── P1 — Update orders from WS events directly ─────────────
                    handleOrdersMessage(data);
                } else if (ch.startsWith('portfolio:')) {
                    // Portfolio pushed from server
                    if (data.balances) {
                        renderPortfolio(data.balances);
                    }
                }
            } catch(err) { console.warn('Mux WS parse error:', err); }
        };
        sock.onerror = () => {
            updateConnectionStatus('disconnected');
        };
        sock.onclose = () => {
            updateConnectionStatus('fallback');
            // Fall back to REST polling if WS drops
            if (currentPair === pairId) {
                startFallbackPoll('orderbook', () => loadOrderbook(pairId), 2500);
                startFallbackPoll('trades', () => loadTrades(pairId), 2500);
                startFallbackPoll('orders', () => loadOpenOrders(), 5000);
                startFallbackPoll('portfolio', () => loadPortfolio(), 5000);
                
                // ── P2 — Exponential backoff reconnect ─────────────────────
                const delay = reconnectDelay * (0.5 + Math.random() * 0.5); // Add jitter
                reconnectTimeout = setTimeout(() => {
                    reconnectDelay = Math.min(reconnectDelay * 2, MAX_RECONNECT_DELAY);
                    if (currentPair === pairId) connectMuxWS(wsBase, pairId);
                }, delay);
            }
        };
    } catch(e) {
        updateConnectionStatus('disconnected');
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

// ── P2 — Connection status indicator ────────────────────────────────────────
function updateConnectionStatus(status) {
    connStatus = status;
    const statusEl = document.getElementById('conn-status');
    if (statusEl) {
        statusEl.className = `conn-dot ${status}`;
        const tooltips = {
            connected: 'WebSocket connected',
            fallback: 'REST polling fallback',
            disconnected: 'Disconnected'
        };
        statusEl.title = tooltips[status] || status;
    }
}

// ── P1 — Handle orders WS messages directly ─────────────────────────────────
let localOrdersList = [];
let lastFullOrdersRefresh = 0;

function handleOrdersMessage(data) {
    if (data.orders && Array.isArray(data.orders)) {
        // Full order list from server
        localOrdersList = data.orders.filter(o => o.pair_id === currentPair || !currentPair);
        renderOpenOrders(localOrdersList);
        lastFullOrdersRefresh = Date.now();
    } else if (data.type) {
        // Order event - update local state
        if (data.type === 'order_created' && data.order) {
            const order = data.order;
            if (order.pair_id === currentPair) {
                localOrdersList.unshift(order);
                renderOpenOrders(localOrdersList);
            }
        } else if (data.type === 'order_cancelled' && data.order_id) {
            localOrdersList = localOrdersList.filter(o => o.id !== data.order_id);
            renderOpenOrders(localOrdersList);
        } else if (data.type === 'orders_cancelled_all') {
            localOrdersList = [];
            renderOpenOrders(localOrdersList);
        } else {
            // For other events, do a full refresh if it's been >30s since last one
            if (Date.now() - lastFullOrdersRefresh > 30000) {
                loadOpenOrders();
            }
        }
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

// ── P3 — Use cached formatters instead of toLocaleString ──────────────────
function fmtPrice(n) {
    if (n == null || isNaN(n)) return '—';
    if (n >= 1000) return fmtPrice2.format(n);
    if (n >= 1)    return fmtPrice4.format(n);
    return fmtPrice6.format(n);
}

function fmtQty(n) {
    if (n == null || isNaN(n)) return '—';
    if (n >= 1000) return fmtQty2.format(n);
    if (n >= 1)    return fmtQty4.format(n);
    return fmtQty5.format(n);
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
