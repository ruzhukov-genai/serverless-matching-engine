// SME Dashboard — polls metrics and updates UI
const API = window.location.origin;
const POLL_INTERVAL = 2000; // ms

async function fetchMetrics() {
    try {
        const [metrics, locks] = await Promise.all([
            fetch(`${API}/api/metrics`).then(r => r.json()),
            fetch(`${API}/api/metrics/locks`).then(r => r.json()),
        ]);

        // KPI cards
        document.getElementById("kpi-orders").textContent = metrics.orders_per_sec;
        document.getElementById("kpi-matches").textContent = metrics.matches_per_sec;
        document.getElementById("kpi-trades").textContent = metrics.trades_per_sec;
        document.getElementById("kpi-pairs").textContent = metrics.active_pairs;
        document.getElementById("kpi-workers").textContent = metrics.active_workers;

        // Lock metrics
        document.getElementById("lock-contention").textContent = (locks.contention_rate * 100).toFixed(1) + "%";
        document.getElementById("lock-wait").textContent = locks.avg_wait_ms.toFixed(1) + " ms";
        document.getElementById("lock-retries").textContent = locks.retry_count;
        document.getElementById("lock-failures").textContent = locks.failures;

        // TODO: update charts, stream depths, audit log
    } catch (e) {
        console.error("Metrics fetch failed:", e);
    }
}

// Poll
setInterval(fetchMetrics, POLL_INTERVAL);
fetchMetrics();
