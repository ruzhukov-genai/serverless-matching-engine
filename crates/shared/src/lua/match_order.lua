-- match_order.lua
-- Atomically matches an incoming order against the order book.
--
-- Dragonfly (and Redis Cluster) require all accessed keys to be declared upfront
-- in KEYS[]. Because we cannot call ZRANGEBYSCORE inside the script and then
-- access the resulting order hashes dynamically, the caller pre-fetches the
-- resting order IDs in a ZRANGEBYSCORE call, then passes the full order:* hash
-- keys in KEYS[5..] for Dragonfly's key-access enforcement.
--
-- KEYS[1] = book:{pair_id}:bids
-- KEYS[2] = book:{pair_id}:asks
-- KEYS[3] = version:{pair_id}
-- KEYS[4] = order:{incoming_order_id}   (for writing resting incoming order)
-- KEYS[5..5+N-1] = order:{resting_id_i} for each of the N candidates
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
-- ARGV[12] = N (number of resting order candidates)
-- ARGV[13..12+N] = resting order ID strings (matched 1-to-1 with KEYS[5..4+N])
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
local n_resting  = tonumber(ARGV[12]) or 0

-- Determine opposite-side and own-side book keys
local opp_key, own_key
if side == "B" then
    opp_key = KEYS[2]  -- asks
    own_key = KEYS[1]  -- bids
else
    opp_key = KEYS[1]  -- bids
    own_key = KEYS[2]  -- asks
end

local remaining_i = qty_i
local status = "N"
local trades = {}

-- ── Match loop over pre-fetched resting order candidates ────────────────────
for i = 1, n_resting do
    if remaining_i <= 0 then break end

    -- KEYS[4+i] = order:{resting_id_i}  (pre-declared by caller)
    -- ARGV[12+i] = resting_id string
    local rkey = KEYS[4 + i]
    local rid  = ARGV[12 + i]

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
        goto continue
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
        break  -- book is sorted by best price; no further matches possible
    end

    -- ── Self-trade prevention (STP) ──────────────────────────────────────
    if user_id == r_user_id then
        local combined = stp_mode
        if combined == 'N' then combined = r_stp end

        if combined == 'CM' then
            redis.call('ZREM', opp_key, rid)
            redis.call('DEL', rkey)
            goto continue
        elseif combined == 'CT' then
            status = 'C'
            remaining_i = 0
            break
        elseif combined == 'CB' then
            redis.call('ZREM', opp_key, rid)
            redis.call('DEL', rkey)
            status = 'C'
            remaining_i = 0
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

    ::continue::
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
