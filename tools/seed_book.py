#!/usr/bin/env python3
"""Seed realistic order books for SME demo."""

import requests
import random
import time
from decimal import Decimal, ROUND_DOWN

API = "http://localhost:3001"

# Config aligned with DB pair configs
PAIRS = {
    "BTC-USDT": {"price": 87000.00, "tick": 0.01, "lot": 0.00001, "levels": 25, "base_qty": 0.05, "spread_pct": 0.0008},
    "ETH-USDT": {"price": 1950.00,  "tick": 0.01, "lot": 0.0001,  "levels": 20, "base_qty": 0.5,  "spread_pct": 0.001},
    "SOL-USDT": {"price": 132.00,   "tick": 0.001,"lot": 0.01,    "levels": 20, "base_qty": 5.0,  "spread_pct": 0.002},
}

USERS = ["mm-1", "mm-2", "mm-3", "mm-4", "mm-5"]

def round_to_tick(price, tick):
    d = Decimal(str(price))
    t = Decimal(str(tick))
    return float((d / t).quantize(Decimal('1'), rounding=ROUND_DOWN) * t)

def round_to_lot(qty, lot):
    d = Decimal(str(qty))
    l = Decimal(str(lot))
    return float((d / l).quantize(Decimal('1'), rounding=ROUND_DOWN) * l)

def place_order(pair_id, side, price, quantity, user_id):
    body = {
        "pair_id": pair_id,
        "side": side,
        "order_type": "Limit",
        "price": str(price),
        "quantity": str(quantity),
        "tif": "GTC",
        "user_id": user_id,
    }
    try:
        r = requests.post(f"{API}/api/orders", json=body, timeout=5)
        if r.status_code not in (200, 201):
            data = r.json() if 'json' in r.headers.get('content-type','') else {}
            err = data.get('error', r.text[:80])
            print(f"    FAIL {r.status_code}: {side} {pair_id} @ {price} x {quantity}: {err}")
        return r.status_code in (200, 201)
    except Exception as e:
        print(f"    ERR: {e}")
        return False

def seed_pair(pair_id, config):
    price = config["price"]
    tick = config["tick"]
    lot = config["lot"]
    levels = config["levels"]
    base_qty = config["base_qty"]
    spread_pct = config["spread_pct"]

    half_spread = price * spread_pct / 2
    best_bid = round_to_tick(price - half_spread, tick)
    best_ask = round_to_tick(price + half_spread + tick, tick)  # ensure > mid

    success = 0
    total = 0

    print(f"\nSeeding {pair_id}: mid={price}, best_bid={best_bid}, best_ask={best_ask}")

    # Bids — decreasing price
    for i in range(levels):
        # Each level is 1-5 ticks apart (clustered near top, spread out deeper)
        gap_ticks = random.randint(1, 2) if i < 5 else random.randint(2, 8)
        level_price = round_to_tick(best_bid - i * gap_ticks * tick, tick)
        if level_price <= 0:
            break

        depth_mult = 1.0 + (i / levels) * 4
        num_orders = random.choices([1, 2, 3], weights=[40, 40, 20])[0]
        for _ in range(num_orders):
            qty = round_to_lot(base_qty * depth_mult * random.uniform(0.5, 1.5), lot)
            if qty <= 0:
                continue
            total += 1
            if place_order(pair_id, "Buy", level_price, qty, random.choice(USERS)):
                success += 1

    # Asks — increasing price
    for i in range(levels):
        gap_ticks = random.randint(1, 2) if i < 5 else random.randint(2, 8)
        level_price = round_to_tick(best_ask + i * gap_ticks * tick, tick)

        depth_mult = 1.0 + (i / levels) * 4
        num_orders = random.choices([1, 2, 3], weights=[40, 40, 20])[0]
        for _ in range(num_orders):
            qty = round_to_lot(base_qty * depth_mult * random.uniform(0.5, 1.5), lot)
            if qty <= 0:
                continue
            total += 1
            if place_order(pair_id, "Sell", level_price, qty, random.choice(USERS)):
                success += 1

    print(f"  Placed: {success}/{total}")
    return success

def seed_trades(pair_id, config):
    """Place some crossing orders to generate recent trade history."""
    price = config["price"]
    tick = config["tick"]
    lot = config["lot"]
    base_qty = config["base_qty"]

    print(f"  Generating trades for {pair_id}...")
    for i in range(15):
        side = random.choice(["Buy", "Sell"])
        # Price that will cross the spread
        if side == "Buy":
            p = round_to_tick(price * (1 + random.uniform(0, 0.002)), tick)
        else:
            p = round_to_tick(price * (1 - random.uniform(0, 0.002)), tick)
        qty = round_to_lot(base_qty * random.uniform(0.1, 0.5), lot)
        if qty <= 0:
            continue
        user = random.choice(["taker-1", "taker-2", "taker-3"])
        place_order(pair_id, side, p, qty, user)
        time.sleep(0.05)

def main():
    print("SME Order Book Seeder\n" + "="*50)

    # Seed balances via docker
    import subprocess
    all_users = USERS + ["taker-1", "taker-2", "taker-3"]
    for user in all_users:
        sql = "; ".join([
            f"INSERT INTO balances (user_id, asset, available, locked) VALUES ('{user}', '{asset}', {amount}, 0) ON CONFLICT (user_id, asset) DO UPDATE SET available = EXCLUDED.available, locked = 0"
            for asset, amount in [("USDT", "10000000"), ("BTC", "100"), ("ETH", "1000"), ("SOL", "10000")]
        ])
        subprocess.run(["docker", "exec", "sme-postgres", "psql", "-U", "sme", "-d", "matching_engine", "-c", sql + ";"], capture_output=True)

    total = 0
    for pair_id, config in PAIRS.items():
        total += seed_pair(pair_id, config)

    # Generate some trades by placing crossing orders
    for pair_id, config in PAIRS.items():
        seed_trades(pair_id, config)

    print(f"\n{'='*50}")
    print(f"Seeding complete! {total} resting orders placed.")
    for pair_id in PAIRS:
        r = requests.get(f"{API}/api/orderbook/{pair_id}")
        data = r.json()
        print(f"  {pair_id}: {len(data.get('bids',[]))} bid levels, {len(data.get('asks',[]))} ask levels")

if __name__ == "__main__":
    main()
