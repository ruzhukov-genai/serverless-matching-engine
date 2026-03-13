//! Order Book cache operations — sorted sets for bids/asks.
//!
//! Keys:
//!   book:{pair_id}:bids  → ZSet (score = price)
//!   book:{pair_id}:asks  → ZSet (score = price)
//!   order:{order_id}     → Hash

// TODO: load_order_book, update_book, get_order, set_order
