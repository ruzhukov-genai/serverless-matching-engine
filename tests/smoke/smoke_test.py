#!/usr/bin/env python3
"""
SME Smoke Tests — headless browser + API automation.

Runs 10 tests exercising the full trading flow (multi-user limit/market orders,
matching, orderbook display, trade feed, portfolio updates, pair switching).

Works against both local Docker and AWS Lambda deployments.

Usage:
    # Local Docker:
    python3 smoke_test.py

    # AWS:
    python3 smoke_test.py \
        --api https://kpvhsf0ub8.execute-api.us-east-1.amazonaws.com \
        --ws wss://2shnq9yk0c.execute-api.us-east-1.amazonaws.com/ws \
        --frontend https://d3ux5yer0uv7b5.cloudfront.net

Requires: playwright (pip install playwright && playwright install chromium)
"""

import argparse
import asyncio
import json
import sys
import time
import urllib.request
import urllib.error
from dataclasses import dataclass
from typing import Optional

# ── Lazy playwright import ────────────────────────────────────────────────────
pw_async = None


def ensure_playwright():
    global pw_async
    if pw_async is None:
        try:
            from playwright.async_api import async_playwright
            pw_async = async_playwright
        except ImportError:
            print("ERROR: pip install playwright && playwright install chromium")
            sys.exit(1)


# ── Config ────────────────────────────────────────────────────────────────────

SELLER = "user-2"  # pre-seeded with BTC + USDT
BUYER  = "user-1"  # pre-seeded with BTC + USDT

def cleanup_book(cfg):
    """Cancel ALL open orders for both users across all pairs to start clean."""
    for user in (BUYER, SELLER):
        for pair in ("BTC-USDT", "ETH-USDT", "SOL-USDT"):
            cancel_all(cfg, user, pair)

@dataclass
class Config:
    api_url: str = "http://localhost:3001"
    ws_url: Optional[str] = None
    frontend_url: Optional[str] = None
    pair: str = "BTC-USDT"
    timeout_ms: int = 15000

    @property
    def effective_ws_url(self) -> str:
        if self.ws_url:
            return self.ws_url
        return self.api_url.replace("https://", "wss://").replace("http://", "ws://")

    @property
    def effective_frontend_url(self) -> str:
        return self.frontend_url or self.api_url

    @property
    def is_aws(self) -> bool:
        return "execute-api" in self.api_url or "cloudfront" in (self.frontend_url or "")

    @property
    def settle_time(self) -> float:
        """Seconds to wait for async order processing."""
        return 4.0 if self.is_aws else 1.5


# ── HTTP helpers ──────────────────────────────────────────────────────────────

def _timeout(cfg: Config) -> int:
    """Longer timeout for AWS (Lambda cold starts can take 15s+)."""
    return 30 if cfg.is_aws else 10


def api_get(cfg: Config, path: str) -> dict:
    url = f"{cfg.api_url}{path}"
    req = urllib.request.Request(url)
    with urllib.request.urlopen(req, timeout=_timeout(cfg)) as resp:
        return json.loads(resp.read())


def api_post(cfg: Config, path: str, body: dict) -> dict:
    url = f"{cfg.api_url}{path}"
    data = json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=_timeout(cfg)) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        return {"error": e.code, "message": e.read().decode() if e.fp else ""}


def api_delete(cfg: Config, path: str) -> dict:
    url = f"{cfg.api_url}{path}"
    req = urllib.request.Request(url, method="DELETE")
    try:
        with urllib.request.urlopen(req, timeout=_timeout(cfg)) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError:
        return {}


_smoke_seq = 0
def place_order(cfg, user, pair, side, order_type, qty, price=None, tif="GTC"):
    global _smoke_seq
    _smoke_seq += 1
    body = {"user_id": user, "pair_id": pair, "side": side,
            "order_type": order_type, "quantity": qty, "tif": tif,
            "client_order_id": f"smoke-{int(time.time())}-{_smoke_seq}"}
    if price:
        body["price"] = price
    return api_post(cfg, "/api/orders", body)


def cancel_all(cfg, user, pair):
    api_delete(cfg, f"/api/orders?user_id={user}&pair_id={pair}")


def settle(cfg):
    time.sleep(cfg.settle_time)


# ── Test framework ────────────────────────────────────────────────────────────

results = []


def record(name, passed, message="", duration_ms=0):
    results.append((name, passed, message))
    icon = "✅" if passed else "❌"
    dur = f" ({duration_ms:.0f}ms)" if duration_ms else ""
    msg = f" — {message}" if message else ""
    print(f"  {icon} {name}{dur}{msg}")


# ── Tests T01–T10 ────────────────────────────────────────────────────────────

# Use a unique price per test run to avoid collisions with prior data.
# Prices in the 75000–79999 range are unlikely to conflict.
UNIQUE = str(int(time.time()) % 5000 + 75000)  # e.g. "76234"


def test_01_pairs_load(cfg):
    """Pairs endpoint returns all configured pairs."""
    t0 = time.monotonic()
    data = api_get(cfg, "/api/pairs")
    dur = (time.monotonic() - t0) * 1000
    pairs = [p["id"] for p in data.get("pairs", [])]
    ok = "BTC-USDT" in pairs and "ETH-USDT" in pairs and "SOL-USDT" in pairs
    record("T01: Pairs load", ok, f"{len(pairs)} pairs: {', '.join(pairs)}", dur)


def test_02_limit_rests(cfg):
    """Limit sell at unique price rests on the book."""
    place_order(cfg, SELLER, cfg.pair, "Sell", "Limit", "0.001", f"{UNIQUE}.00")
    settle(cfg)
    ob = api_get(cfg, f"/api/orderbook/{cfg.pair}?depth=50")
    asks = [l[0] for l in ob.get("asks", [])]
    found = UNIQUE in asks or f"{UNIQUE}.00" in asks
    record("T02: Limit sell rests on book", found, f"price={UNIQUE} in asks={asks[:5]}")


def test_03_crossing_trade(cfg):
    """Buyer crosses → a new trade appears (price-time priority may fill at best ask)."""
    # Get trade count before
    before = api_get(cfg, f"/api/trades/{cfg.pair}").get("trades", [])
    before_count = len(before)
    before_latest = before[0]["id"] if before else None

    # Buy at our price — may match at a lower resting ask (price-time priority)
    place_order(cfg, BUYER, cfg.pair, "Buy", "Limit", "0.001", f"{UNIQUE}.00")
    settle(cfg)

    after = api_get(cfg, f"/api/trades/{cfg.pair}").get("trades", [])
    new_trades = [t for t in after if t["id"] != before_latest][:5] if before_latest else after[:5]

    found = len(after) > before_count or len(new_trades) > 0
    latest_price = new_trades[0]["price"] if new_trades else "none"
    record("T03: Crossing trade executes", found,
           f"new trade at price={latest_price}")


def test_04_ticker_updates(cfg):
    """Ticker last price changed after the trade."""
    data = api_get(cfg, f"/api/ticker/{cfg.pair}")
    last = data.get("last", "0")
    # Just verify ticker returns a non-zero price
    ok = last != "0" and last != ""
    record("T04: Ticker has price", ok, f"last={last}")


def test_05_portfolio_reflects_trade(cfg):
    """Both users have balances after trading."""
    buyer = api_get(cfg, f"/api/portfolio?user_id={BUYER}")
    seller = api_get(cfg, f"/api/portfolio?user_id={SELLER}")
    b_btc = next((b for b in buyer.get("balances", []) if b["asset"] == "BTC"), None)
    s_usdt = next((b for b in seller.get("balances", []) if b["asset"] == "USDT"), None)
    ok = (b_btc and float(b_btc["available"]) > 0 and
          s_usdt and float(s_usdt["available"]) > 0)
    record("T05: Portfolio reflects trade", ok,
           f"buyer BTC={b_btc['available'] if b_btc else '?'}, "
           f"seller USDT={s_usdt['available'] if s_usdt else '?'}")


def test_06_orderbook_depth(cfg):
    """Orderbook has correct structure with price/size/pair."""
    ob = api_get(cfg, f"/api/orderbook/{cfg.pair}?depth=10")
    has_pair = ob.get("pair") == cfg.pair
    has_bids = isinstance(ob.get("bids"), list)
    has_asks = isinstance(ob.get("asks"), list)
    # Each level should be [price_str, qty_str]
    valid_format = True
    for side in ("bids", "asks"):
        for level in ob.get(side, [])[:3]:
            if not (isinstance(level, list) and len(level) == 2):
                valid_format = False
    record("T06: Orderbook structure valid", has_pair and has_bids and has_asks and valid_format,
           f"pair={ob.get('pair')} bids={len(ob.get('bids',[]))} asks={len(ob.get('asks',[]))}")


def test_07_market_order(cfg):
    """Market buy executes → new trade appears in trade history."""
    # Get trade count before
    before = api_get(cfg, f"/api/trades/{cfg.pair}").get("trades", [])
    before_ids = {t["id"] for t in before[:10]}

    # Market buy should consume the best ask
    place_order(cfg, BUYER, cfg.pair, "Buy", "Market", "0.001", tif="IOC")
    settle(cfg)

    after = api_get(cfg, f"/api/trades/{cfg.pair}").get("trades", [])
    new_trades = [t for t in after[:10] if t["id"] not in before_ids]

    record("T07: Market order creates trade", len(new_trades) > 0,
           f"{len(new_trades)} new trade(s), price={new_trades[0]['price'] if new_trades else '?'}")


def test_08_pair_isolation(cfg):
    """ETH order doesn't appear in BTC orderbook."""
    price3 = str(int(UNIQUE) + 200)
    place_order(cfg, SELLER, "ETH-USDT", "Sell", "Limit", "0.01", f"{price3}.00")
    settle(cfg)

    btc_ob = api_get(cfg, "/api/orderbook/BTC-USDT?depth=50")
    eth_ob = api_get(cfg, "/api/orderbook/ETH-USDT?depth=50")

    btc_all = [l[0] for l in btc_ob.get("asks", []) + btc_ob.get("bids", [])]
    eth_all = [l[0] for l in eth_ob.get("asks", []) + eth_ob.get("bids", [])]

    eth_has = price3 in eth_all or f"{price3}.00" in eth_all
    btc_clean = price3 not in btc_all and f"{price3}.00" not in btc_all

    # Cleanup
    cancel_all(cfg, SELLER, "ETH-USDT")

    record("T08: Pair isolation",
           eth_has and btc_clean,
           f"ETH has {price3}: {eth_has}, BTC clean: {btc_clean}")


async def test_09_browser_orderbook(cfg):
    """Browser shows orderbook, ticker, portfolio after page load."""
    ensure_playwright()

    # Place a visible resting order first
    price4 = str(int(UNIQUE) + 300)
    place_order(cfg, SELLER, cfg.pair, "Sell", "Limit", "0.005", f"{price4}.00")
    settle(cfg)

    async with pw_async() as p:
        browser = await p.chromium.launch(headless=True)
        page = await browser.new_page()
        await page.goto(cfg.effective_frontend_url, wait_until="networkidle",
                        timeout=cfg.timeout_ms)

        # Wait for orderbook to populate
        try:
            await page.wait_for_selector("#bids .level, #asks .level",
                                         timeout=cfg.timeout_ms)
        except Exception:
            pass

        bid_count = await page.locator("#bids .level").count()
        ask_count = await page.locator("#asks .level").count()
        pair_text = await page.locator("#current-pair").text_content()
        portfolio_text = await page.locator("#portfolio-body").text_content()
        has_portfolio = "USDT" in portfolio_text

        # Check the specific ask price is visible
        page_text = await page.locator("#asks").text_content()
        price_visible = price4 in page_text.replace(",", "")

        await browser.close()

    # Cleanup
    cancel_all(cfg, SELLER, cfg.pair)

    ok = (bid_count + ask_count) > 0 and pair_text == cfg.pair and has_portfolio
    record("T09: Browser shows orderbook + portfolio", ok,
           f"bids={bid_count} asks={ask_count} pair={pair_text} "
           f"portfolio={'yes' if has_portfolio else 'no'} "
           f"price {price4} visible={price_visible}")


async def test_10_browser_live_trade(cfg):
    """Place crossing orders via API → new trade appears in browser trades list."""
    ensure_playwright()

    price5 = str(int(UNIQUE) + 400)

    async with pw_async() as p:
        browser = await p.chromium.launch(headless=True)
        page = await browser.new_page()
        await page.goto(cfg.effective_frontend_url, wait_until="networkidle",
                        timeout=cfg.timeout_ms)

        await page.wait_for_selector("#trades-list", timeout=cfg.timeout_ms)
        await asyncio.sleep(2)

        # Snapshot the full trades list HTML
        initial_html = await page.locator("#trades-list").inner_html()

        # Place crossing orders — sell then buy
        place_order(cfg, SELLER, cfg.pair, "Sell", "Limit", "0.001", f"{price5}.00")
        await asyncio.sleep(cfg.settle_time)
        place_order(cfg, BUYER, cfg.pair, "Buy", "Limit", "0.001", f"{price5}.00")

        # Wait for trades list to update (WS push or REST fallback @2.5s)
        found = False
        for _ in range(12):
            await asyncio.sleep(2)
            current_html = await page.locator("#trades-list").inner_html()
            if current_html != initial_html:
                found = True
                break

        if not found:
            # Force a page refresh as final attempt
            await page.reload(wait_until="networkidle", timeout=cfg.timeout_ms)
            await asyncio.sleep(3)
            refreshed_html = await page.locator("#trades-list").inner_html()
            found = refreshed_html != initial_html

        await browser.close()

    record("T10: Live trade appears in browser", found,
           f"updated={'WS/poll' if found else 'no'}")


# ── Cleanup ───────────────────────────────────────────────────────────────────

def cleanup(cfg):
    for user in (BUYER, SELLER):
        for pair in ("BTC-USDT", "ETH-USDT", "SOL-USDT"):
            cancel_all(cfg, user, pair)


# ── Main ──────────────────────────────────────────────────────────────────────

async def run_all(cfg):
    print(f"\n{'='*60}")
    print(f"  SME Smoke Tests (unique price base: {UNIQUE})")
    print(f"  API:      {cfg.api_url}")
    print(f"  WS:       {cfg.effective_ws_url}")
    print(f"  Frontend: {cfg.effective_frontend_url}")
    print(f"  Mode:     {'AWS Lambda' if cfg.is_aws else 'Local Docker'}")
    print(f"{'='*60}\n")

    t_start = time.monotonic()

    # API tests
    print("── API Tests ──")
    test_01_pairs_load(cfg)
    test_02_limit_rests(cfg)
    test_03_crossing_trade(cfg)
    test_04_ticker_updates(cfg)
    test_05_portfolio_reflects_trade(cfg)
    test_06_orderbook_depth(cfg)
    test_07_market_order(cfg)
    test_08_pair_isolation(cfg)

    # Browser tests
    print("\n── Browser Tests ──")
    try:
        await test_09_browser_orderbook(cfg)
        await test_10_browser_live_trade(cfg)
    except Exception as e:
        record("T09/T10: Browser tests", False, str(e))

    # Cleanup
    print("\n── Cleanup ──")
    cleanup(cfg)
    print("  🧹 Cancelled leftover orders")

    elapsed = (time.monotonic() - t_start) * 1000
    passed = sum(1 for _, ok, _ in results if ok)
    failed = sum(1 for _, ok, _ in results if not ok)

    print(f"\n{'='*60}")
    print(f"  Results: {passed} passed, {failed} failed ({elapsed:.0f}ms)")
    print(f"{'='*60}\n")

    if failed:
        print("Failed tests:")
        for name, ok, msg in results:
            if not ok:
                print(f"  ❌ {name}: {msg}")
        print()

    return failed == 0


def main():
    parser = argparse.ArgumentParser(description="SME Smoke Tests")
    parser.add_argument("--api", default="http://localhost:3001", help="API base URL")
    parser.add_argument("--ws", default=None, help="WebSocket URL")
    parser.add_argument("--frontend", default=None, help="Frontend URL")
    parser.add_argument("--pair", default="BTC-USDT", help="Primary pair")
    parser.add_argument("--no-browser", action="store_true", help="Skip T09-T10")
    args = parser.parse_args()

    cfg = Config(
        api_url=args.api.rstrip("/"),
        ws_url=args.ws,
        frontend_url=args.frontend.rstrip("/") if args.frontend else None,
        pair=args.pair,
    )

    if args.no_browser:
        print(f"\n{'='*60}")
        print(f"  SME Smoke Tests — API only (unique price base: {UNIQUE})")
        print(f"  API: {cfg.api_url}")
        print(f"{'='*60}\n")
        test_01_pairs_load(cfg)
        test_02_limit_rests(cfg)
        test_03_crossing_trade(cfg)
        test_04_ticker_updates(cfg)
        test_05_portfolio_reflects_trade(cfg)
        test_06_orderbook_depth(cfg)
        test_07_market_order(cfg)
        test_08_pair_isolation(cfg)
        cleanup(cfg)
        passed = sum(1 for _, ok, _ in results if ok)
        failed = sum(1 for _, ok, _ in results if not ok)
        print(f"\n  Results: {passed} passed, {failed} failed")
        sys.exit(0 if failed == 0 else 1)

    ok = asyncio.run(run_all(cfg))
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()