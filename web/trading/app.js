// SME Trading UI — client-side application
// Connects to REST API + WebSocket feeds

const API = window.location.origin;
let currentPair = null;
let currentSide = "buy";
let ws = { orderbook: null, trades: null };

// ── Initialization ───────────────────────────────────────────────────────────

async function init() {
    await loadPairs();
    setupOrderForm();
    setupTabs();
}

// ── Pairs ────────────────────────────────────────────────────────────────────

async function loadPairs() {
    try {
        const res = await fetch(`${API}/api/pairs`);
        const data = await res.json();
        renderPairs(data.pairs || []);
        if (data.pairs?.length > 0) selectPair(data.pairs[0].id);
    } catch (e) {
        console.error("Failed to load pairs:", e);
    }
}

function renderPairs(pairs) {
    const el = document.getElementById("pairs-list");
    el.innerHTML = pairs.map(p =>
        `<button data-pair="${p.id}" onclick="selectPair('${p.id}')">${p.base}/${p.quote}</button>`
    ).join("");
}

function selectPair(pairId) {
    currentPair = pairId;
    document.getElementById("current-pair").textContent = pairId;
    document.querySelectorAll("#pairs-list button").forEach(b => {
        b.classList.toggle("active", b.dataset.pair === pairId);
    });
    loadOrderbook(pairId);
    loadTrades(pairId);
    connectWebSockets(pairId);
}

// ── Order Book ───────────────────────────────────────────────────────────────

async function loadOrderbook(pairId) {
    try {
        const res = await fetch(`${API}/api/orderbook/${pairId}`);
        const data = await res.json();
        renderOrderbook(data);
    } catch (e) {
        console.error("Failed to load orderbook:", e);
    }
}

function renderOrderbook(data) {
    // TODO: render bid/ask levels
}

// ── Trades ───────────────────────────────────────────────────────────────────

async function loadTrades(pairId) {
    try {
        const res = await fetch(`${API}/api/trades/${pairId}`);
        const data = await res.json();
        renderTrades(data.trades || []);
    } catch (e) {
        console.error("Failed to load trades:", e);
    }
}

function renderTrades(trades) {
    // TODO: render trade rows
}

// ── WebSocket ────────────────────────────────────────────────────────────────

function connectWebSockets(pairId) {
    const wsBase = API.replace(/^http/, "ws");

    if (ws.orderbook) ws.orderbook.close();
    ws.orderbook = new WebSocket(`${wsBase}/ws/orderbook/${pairId}`);
    ws.orderbook.onmessage = (e) => {
        const update = JSON.parse(e.data);
        // TODO: apply incremental update to orderbook
    };

    if (ws.trades) ws.trades.close();
    ws.trades = new WebSocket(`${wsBase}/ws/trades/${pairId}`);
    ws.trades.onmessage = (e) => {
        const trade = JSON.parse(e.data);
        // TODO: prepend to trades list
    };
}

// ── Order Entry ──────────────────────────────────────────────────────────────

function setupTabs() {
    document.querySelectorAll(".tab").forEach(tab => {
        tab.addEventListener("click", () => {
            currentSide = tab.dataset.side;
            document.querySelectorAll(".tab").forEach(t => t.classList.remove("active"));
            tab.classList.add("active");
            const btn = document.getElementById("submit-order");
            btn.textContent = currentSide === "buy" ? "Buy" : "Sell";
            btn.className = currentSide === "buy" ? "btn-buy" : "btn-buy btn-sell";
        });
    });
}

function setupOrderForm() {
    const form = document.getElementById("order-form");
    const typeSelect = document.getElementById("order-type");
    const priceGroup = document.getElementById("price-group");

    typeSelect.addEventListener("change", () => {
        priceGroup.style.display = typeSelect.value === "market" ? "none" : "block";
    });

    // Calculate total
    const priceInput = document.getElementById("order-price");
    const qtyInput = document.getElementById("order-quantity");
    const totalSpan = document.getElementById("order-total");

    function updateTotal() {
        const price = parseFloat(priceInput.value) || 0;
        const qty = parseFloat(qtyInput.value) || 0;
        totalSpan.textContent = (price * qty).toFixed(2);
    }
    priceInput.addEventListener("input", updateTotal);
    qtyInput.addEventListener("input", updateTotal);

    form.addEventListener("submit", async (e) => {
        e.preventDefault();
        await submitOrder();
    });
}

async function submitOrder() {
    const order = {
        pair_id: currentPair,
        side: currentSide,
        order_type: document.getElementById("order-type").value,
        price: document.getElementById("order-type").value === "limit"
            ? document.getElementById("order-price").value
            : null,
        quantity: document.getElementById("order-quantity").value,
        tif: document.getElementById("order-tif").value,
    };

    try {
        const res = await fetch(`${API}/api/orders`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(order),
        });
        const data = await res.json();
        console.log("Order submitted:", data);
        loadPortfolio();
        loadOpenOrders();
    } catch (e) {
        console.error("Failed to submit order:", e);
    }
}

// ── Portfolio ────────────────────────────────────────────────────────────────

async function loadPortfolio() {
    try {
        const res = await fetch(`${API}/api/portfolio`);
        const data = await res.json();
        // TODO: render balances
    } catch (e) {
        console.error("Failed to load portfolio:", e);
    }
}

// ── Open Orders ──────────────────────────────────────────────────────────────

async function loadOpenOrders() {
    try {
        const res = await fetch(`${API}/api/orders`);
        const data = await res.json();
        // TODO: render open orders with cancel buttons
    } catch (e) {
        console.error("Failed to load orders:", e);
    }
}

async function cancelOrder(orderId) {
    try {
        await fetch(`${API}/api/orders/${orderId}`, { method: "DELETE" });
        loadOpenOrders();
    } catch (e) {
        console.error("Failed to cancel order:", e);
    }
}

// ── Start ────────────────────────────────────────────────────────────────────

init();
