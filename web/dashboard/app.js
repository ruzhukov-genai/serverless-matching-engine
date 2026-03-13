// SME Dashboard — real-data polling from /api/metrics/* and /api/audit
'use strict';

const API = window.location.origin;
const POLL_INTERVAL = 2000;
const CHART_POINTS = 60;

// Time-series data buffers
const throughputData = { labels: [], values: [] };
const latencyData = { labels: [], p50: [], p95: [], p99: [] };

// ── Init ──────────────────────────────────────────────────────────────────────

function init() {
    fetchMetrics();
    fetchAudit();
    setInterval(fetchMetrics, POLL_INTERVAL);
    setInterval(fetchAudit, 3000);
}

// ── KPI + Charts ──────────────────────────────────────────────────────────────

async function fetchMetrics() {
    try {
        const [metricsData, locksData, tpData, latData] = await Promise.all([
            fetchJson('/api/metrics'),
            fetchJson('/api/metrics/locks'),
            fetchJson('/api/metrics/throughput'),
            fetchJson('/api/metrics/latency'),
        ]);

        // KPI cards — live totals
        animateValue('kpi-orders', metricsData.orders_per_sec || 0);
        animateValue('kpi-matches', metricsData.matches_per_sec || 0);
        animateValue('kpi-trades', metricsData.trades_per_sec || 0);
        animateValue('kpi-pairs', metricsData.active_pairs || 0);
        animateValue('kpi-workers', metricsData.active_workers || 0);

        // Lock metrics from real Dragonfly data
        setText('lock-contention', ((locksData.contention_rate || 0) * 100).toFixed(1) + '%');
        setText('lock-wait', (locksData.avg_wait_ms || 0).toFixed(2) + ' ms');
        setText('lock-retries', locksData.retry_count || 0);
        setText('lock-failures', locksData.failures || 0);

        // Stream depths (none in current PoC)
        updateStreamBar('stream-orders', 'stream-orders-bar', 0);
        updateStreamBar('stream-transactions', 'stream-transactions-bar', 0);
        updateStreamBar('stream-stats', 'stream-stats-bar', 0);

        // Throughput chart — real trade buckets from DB
        const now = new Date().toLocaleTimeString('en-US', { hour12: false });
        const series = tpData.series || [];
        // Use latest count from the most recent minute bucket (if any)
        const latestCount = series.length > 0 ? series[series.length - 1].count : 0;
        pushData(throughputData, now, { values: latestCount });
        drawThroughputChart(tpData.series || []);

        // Latency chart — real P50/P95/P99 from Dragonfly samples
        const p50 = latData.p50 || 0;
        const p95 = latData.p95 || 0;
        const p99 = latData.p99 || 0;
        pushData(latencyData, now, { p50, p95, p99 });
        drawLatencyChart();

    } catch (e) {
        console.error('Metrics fetch failed:', e);
    }
}

// ── Audit Log ─────────────────────────────────────────────────────────────────

async function fetchAudit() {
    try {
        const data = await fetchJson('/api/audit');
        // Support both {audit:[]} and {events:[]} shapes
        const events = data.audit || data.events || [];
        const tbody = document.getElementById('audit-body');
        if (!tbody) return;
        if (events.length === 0) {
            tbody.innerHTML = '<tr><td colspan="5" class="empty">No events yet</td></tr>';
            return;
        }
        tbody.innerHTML = events.map(e => {
            const payload = e.payload || e.details || {};
            const detailStr = typeof payload === 'object' ? JSON.stringify(payload) : String(payload);
            return `
            <tr>
                <td>${esc(e.sequence)}</td>
                <td>${fmtTime(e.created_at)}</td>
                <td>${esc(e.pair_id || '—')}</td>
                <td><span class="event-badge event-${eventClass(e.event_type)}">${esc(e.event_type)}</span></td>
                <td class="details">${esc(detailStr)}</td>
            </tr>`;
        }).join('');
    } catch (e) {
        console.error('Audit fetch failed:', e);
    }
}

function eventClass(type) {
    if (!type) return 'default';
    const t = type.toLowerCase();
    if (t.includes('trade') || t.includes('fill')) return 'trade';
    if (t.includes('cancel') || t.includes('reject')) return 'cancel';
    if (t.includes('create') || t.includes('new')) return 'create';
    return 'default';
}

// ── Charts (Canvas) ───────────────────────────────────────────────────────────

function drawThroughputChart(dbSeries) {
    const canvas = document.getElementById('throughput-chart');
    if (!canvas) return;
    const ctx = canvas.getContext('2d');
    const w = canvas.parentElement.clientWidth || 400;
    canvas.width = w;
    canvas.height = 180;
    ctx.clearRect(0, 0, w, 180);

    if (!dbSeries || dbSeries.length === 0) {
        // Use time-series buffer if no DB data yet
        if (throughputData.values.length < 2) return;
        const series = [{ data: throughputData.values, color: '#d29922', label: 'Trades/min' }];
        drawLineChart(ctx, w, 180, series, throughputData.labels);
        return;
    }

    // Plot real DB minute-bucket data
    const labels = dbSeries.map(b => {
        const d = new Date(b.time);
        return d.toLocaleTimeString('en-US', { hour12: false }).slice(0, 5);
    });
    const counts = dbSeries.map(b => b.count);
    const series = [{ data: counts, color: '#d29922', label: 'Trades/min' }];
    drawLineChart(ctx, w, 180, series, labels);
}

function drawLatencyChart() {
    const canvas = document.getElementById('latency-chart');
    if (!canvas) return;
    const ctx = canvas.getContext('2d');
    const w = canvas.parentElement.clientWidth || 400;
    canvas.width = w;
    canvas.height = 180;
    ctx.clearRect(0, 0, w, 180);

    if (latencyData.p50.length < 2) return;

    const series = [
        { data: latencyData.p50, color: '#3fb950', label: 'P50 ms' },
        { data: latencyData.p95, color: '#d29922', label: 'P95 ms' },
        { data: latencyData.p99, color: '#f85149', label: 'P99 ms' },
    ];
    drawLineChart(ctx, w, 180, series, latencyData.labels);
}

function drawLineChart(ctx, w, h, series, labels) {
    const pad = { top: 25, right: 15, bottom: 25, left: 50 };
    const cw = w - pad.left - pad.right;
    const ch = h - pad.top - pad.bottom;

    let max = 1;
    series.forEach(s => s.data.forEach(v => { if (v > max) max = v; }));
    max = Math.ceil(max * 1.2) || 1;

    // Grid
    ctx.strokeStyle = '#21262d';
    ctx.lineWidth = 1;
    for (let i = 0; i <= 4; i++) {
        const y = pad.top + (ch / 4) * i;
        ctx.beginPath();
        ctx.moveTo(pad.left, y);
        ctx.lineTo(w - pad.right, y);
        ctx.stroke();
        ctx.fillStyle = '#8b949e';
        ctx.font = '10px sans-serif';
        ctx.textAlign = 'right';
        const val = max - (max / 4) * i;
        ctx.fillText(val.toFixed(val >= 10 ? 0 : 1), pad.left - 5, y + 3);
    }

    const n = labels.length;
    if (n < 2) return;

    series.forEach(s => {
        ctx.strokeStyle = s.color;
        ctx.lineWidth = 2;
        ctx.beginPath();
        s.data.forEach((v, i) => {
            const x = pad.left + (cw / (n - 1)) * i;
            const y = pad.top + ch - (v / max) * ch;
            if (i === 0) ctx.moveTo(x, y);
            else ctx.lineTo(x, y);
        });
        ctx.stroke();
    });

    // Legend
    let lx = pad.left;
    ctx.font = '11px sans-serif';
    series.forEach(s => {
        ctx.fillStyle = s.color;
        ctx.fillRect(lx, 5, 12, 12);
        ctx.fillStyle = '#e6edf3';
        ctx.textAlign = 'left';
        ctx.fillText(s.label, lx + 16, 15);
        lx += ctx.measureText(s.label).width + 36;
    });

    // X-axis labels
    ctx.fillStyle = '#8b949e';
    ctx.font = '9px sans-serif';
    ctx.textAlign = 'center';
    for (let i = 0; i < n; i += Math.max(1, Math.floor(n / 6))) {
        const x = pad.left + (cw / (n - 1)) * i;
        const lbl = String(labels[i]);
        ctx.fillText(lbl.slice(-5), x, h - 5);
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function pushData(buf, label, vals) {
    buf.labels.push(label);
    Object.keys(vals).forEach(k => {
        if (!buf[k]) buf[k] = [];
        buf[k].push(vals[k]);
    });
    if (buf.labels.length > CHART_POINTS) {
        buf.labels.shift();
        Object.keys(vals).forEach(k => { if (buf[k]) buf[k].shift(); });
    }
}

function updateStreamBar(countId, barId, count) {
    setText(countId, count);
    const maxBar = 100;
    const pct = Math.min(100, (count / maxBar) * 100);
    const bar = document.getElementById(barId);
    if (bar) bar.style.width = pct + '%';
}

function animateValue(id, newVal) {
    const el = document.getElementById(id);
    if (!el) return;
    const cur = parseFloat(el.textContent) || 0;
    if (cur === newVal) return;
    el.textContent = typeof newVal === 'number' && !Number.isInteger(newVal)
        ? newVal.toFixed(1)
        : newVal;
    el.classList.add('flash');
    setTimeout(() => el.classList.remove('flash'), 600);
}

function setText(id, val) {
    const el = document.getElementById(id);
    if (el) el.textContent = val;
}

function fmtTime(ts) {
    if (!ts) return '—';
    const d = new Date(ts);
    return isNaN(d.getTime()) ? '—' : d.toLocaleTimeString('en-US', { hour12: false });
}

function esc(s) {
    if (s == null) return '';
    return String(s)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;');
}

async function fetchJson(path) {
    const res = await fetch(API + path);
    if (!res.ok) throw new Error(`${path} returned ${res.status}`);
    return res.json();
}

// ── Boot ──────────────────────────────────────────────────────────────────────

init();
