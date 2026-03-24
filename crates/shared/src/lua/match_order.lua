-- match_order.lua (v2 — single round-trip)
-- Atomically matches an incoming order against the order book.
--
-- ZRANGEBYSCORE is called INSIDE the script to fetch resting order IDs,
-- eliminating Phase 1 from Rust (saves 1 round-trip). Dragonfly in default
-- mode allows undeclared key access in scripts (no cluster restrictions).
--
-- KEYS[1] = book:{pair_id}:bids
-- KEYS[2] = book:{pair_id}:asks
-- KEYS[3] = version:{pair_id}
-- KEYS[4] = order:{incoming_order_id}   (for writing resting incoming order)
--
-- ARGV[1]  = order_id (string)
-- ARGV[2]  = side: "B" (buy) or "S" (sell)
-- ARGV[3]  = order_type: "L" (limit) or "M" (market)
-- ARGV[4]  = tif: "G" (GTC) or "I" (IOC) or "F" (FOK — handled in Rust pre-check)
-- ARGV[5]  = price_i (price * 10^8, integer string; "0" for market orders)
-- ARGV[6]  = qty_i   (quantity * 10^8, integer string)
-- ARGV[7]  = user_id
-- ARGV[8]  = stp_mode: "N" / "CM" / "CT" / "CB"
-- ARGV[9]  = created_at_ms (integer string)
-- ARGV[10] = pair_id (used when writing resting incoming order hash)
-- ARGV[11] = score (float string, for placing resting incoming order in sorted set)
-- ARGV[12] = max_score (price bound for ZRANGEBYSCORE, e.g. "5000000000" or "+inf")
-- ARGV[13] = lock_asset (asset to lock balance for, e.g. "USDT" or "BTC")
-- ARGV[14] = lock_amount_scaled (amount * 10^8 as i64 string; "0" means no lock)
--
-- Returns array:
--   [1] = "OK"
--   [2] = remaining_i (integer string)
--   [3] = status ("N" / "PF" / "F" / "C")
--   [4] = trade_count (integer string)
--   then for each trade (5 fields):
--   [4 + i*5 + 1] = resting_order_id
--   [4 + i*5 + 2] = price_i (integer string)
--   [4 + i*5 + 3] = qty_i   (integer string)
--   [4 + i*5 + 4] = buyer_id
--   [4 + i*5 + 5] = seller_id
--
-- NOTE: FOK orders are NOT handled here — see match_order_lua in cache.rs.

local order_id   = ARGV[1]
local side       = ARGV[2]
local order_type = ARGV[3]
local tif        = ARGV[4]
local price_i    = tonumber(ARGV[5]) or 0
local qty_i      = tonumber(ARGV[6])
local user_id    = ARGV[7]
local stp_mode   = ARGV[8]
local ts_ms      = ARGV[9]
local pair_id    = ARGV[10]
local score      = ARGV[11]
local max_score  = ARGV[12]

-- ── Balance lock (merged from lock_balance_dragonfly — saves 1 round-trip) ───
-- ARGV[13]/ARGV[14]: lock_asset / lock_amount_scaled
-- Skip if lock_amount is 0 or absent (e.g. market buy with unknown price).
local lock_asset  = ARGV[13]
local lock_amount = tonumber(ARGV[14])
if lock_asset and lock_asset ~= '' and lock_amount and lock_amount > 0 then
    local bal_key = 'balance:' .. user_id .. ':' .. lock_asset
    local avail   = tonumber(redis.call('HGET', bal_key, 'available') or '0')
    if avail < lock_amount then
        return {'INSUFFICIENT_BALANCE'}
    end
    redis.call('HINCRBY', bal_key, 'available', -lock_amount)
    redis.call('HINCRBY', bal_key, 'locked',    lock_amount)
end

-- Determine opposite-side and own-side book keys
local opp_key, own_key
if side == "B" then
    opp_key = KEYS[2]  -- asks
    own_key = KEYS[1]  -- bids
else
    opp_key = KEYS[1]  -- bids
    own_key = KEYS[2]  -- asks
end

-- Fetch resting order IDs from opposite book (price-bounded, up to 50)
-- ZRANGEBYSCORE on KEYS[1]/KEYS[2] is fine — these are declared keys.
local resting_ids = redis.call('ZRANGEBYSCORE', opp_key, '-inf', max_score, 'LIMIT', 0, 50)

local remaining_i = qty_i
local status = "N"
local trades = {}

-- ── Match loop over resting order candidates ─────────────────────────────────
-- NOTE: Uses repeat/until true pattern for "continue" since Redis uses Lua 5.1
-- which lacks goto. `break` inside the repeat block skips to next iteration.
local match_break = false
for _, rid in ipairs(resting_ids) do
    if remaining_i <= 0 or match_break then break end

    repeat  -- "continue" block: break here = skip to next resting order

    -- Access order hash directly — standalone Redis allows undeclared keys in scripts
    local rkey = 'order:' .. rid
    local rfields = redis.call('HMGET', rkey,
        'price_i', 'remaining_i', 'user_id', 'stp', 'status', 'order_type')

    local r_price_i     = tonumber(rfields[1]) or 0
    local r_remaining_i = tonumber(rfields[2]) or 0
    local r_user_id     = rfields[3] or ''
    local r_stp         = rfields[4] or 'N'
    local r_status      = rfields[5] or ''
    local r_order_type  = rfields[6] or 'L'

    -- Skip ghost / already-consumed entries
    if r_status == 'F' or r_status == 'C' or r_remaining_i <= 0 then
        break  -- continue
    end

    -- ── Price compatibility check ────────────────────────────────────────
    local crosses = false
    if order_type == 'M' then
        crosses = true
    elseif side == 'B' then
        crosses = (price_i >= r_price_i)
    else
        crosses = (price_i <= r_price_i)
    end

    if not crosses then
        match_break = true
        break  -- book is sorted by best price; no further matches possible
    end

    -- ── Self-trade prevention (STP) ──────────────────────────────────────
    if user_id == r_user_id then
        local combined = stp_mode
        if combined == 'N' then combined = r_stp end

        if combined == 'CM' then
            redis.call('ZREM', opp_key, rid)
            redis.call('DEL', rkey)
            break  -- continue
        elseif combined == 'CT' then
            status = 'C'
            remaining_i = 0
            match_break = true
            break
        elseif combined == 'CB' then
            redis.call('ZREM', opp_key, rid)
            redis.call('DEL', rkey)
            status = 'C'
            remaining_i = 0
            match_break = true
            break
        end
        -- STP=N with same user: fall through and trade
    end

    -- ── Fill calculation ─────────────────────────────────────────────────
    local fill_i = remaining_i
    if r_remaining_i < fill_i then fill_i = r_remaining_i end

    -- Trade executes at resting price; if resting is also market, use incoming price
    local trade_price_i = r_price_i
    if r_order_type == 'M' then trade_price_i = price_i end

    local buyer_id, seller_id
    if side == 'B' then
        buyer_id  = user_id
        seller_id = r_user_id
    else
        buyer_id  = r_user_id
        seller_id = user_id
    end

    table.insert(trades, {
        resting_id   = rid,
        price_i      = trade_price_i,
        qty_i        = fill_i,
        buyer_id     = buyer_id,
        seller_id    = seller_id,
    })

    remaining_i = remaining_i - fill_i

    local new_r_remaining_i = r_remaining_i - fill_i
    if new_r_remaining_i <= 0 then
        redis.call('ZREM', opp_key, rid)
        redis.call('DEL', rkey)
    else
        redis.call('HSET', rkey,
            'remaining_i', tostring(new_r_remaining_i),
            'status', 'PF')
    end

    until true  -- end "continue" block
end

-- ── Apply TIF rules ──────────────────────────────────────────────────────────

if status ~= 'C' then
    if remaining_i <= 0 then
        status = 'F'
    elseif remaining_i < qty_i then
        status = 'PF'
    else
        status = 'N'
    end
end

-- IOC: cancel remainder
if tif == 'I' and remaining_i > 0 and status ~= 'C' then
    status = 'C'
end

-- GTC with remainder: add incoming order to own book
-- KEYS[4] = order:{incoming_order_id}
if tif == 'G' and remaining_i > 0 and status ~= 'C' then
    redis.call('ZADD', own_key, score, order_id)
    redis.call('HSET', KEYS[4],
        'id',          order_id,
        'pair_id',     pair_id,
        'side',        side,
        'order_type',  order_type,
        'tif',         tif,
        'price_i',     tostring(price_i),
        'qty_i',       tostring(qty_i),
        'remaining_i', tostring(remaining_i),
        'status',      status,
        'stp',         stp_mode,
        'user_id',     user_id,
        'ts_ms',       ts_ms,
        'version',     '1'
    )
end

redis.call('INCR', KEYS[3])

-- ── Build return array ───────────────────────────────────────────────────────
local result = {'OK', tostring(remaining_i), status, tostring(#trades)}
for _, t in ipairs(trades) do
    table.insert(result, t.resting_id)
    table.insert(result, tostring(t.price_i))
    table.insert(result, tostring(t.qty_i))
    table.insert(result, t.buyer_id)
    table.insert(result, t.seller_id)
end

return result
