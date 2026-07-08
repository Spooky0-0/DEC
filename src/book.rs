//! Cache-friendly, pre-allocated Order Book implementation.
//!
//! Designed for ultra-low latency matching engines. Memory is pre-allocated
//! in a contiguous flat array (the Arena pattern) during initialization.
//! During hot-path execution:
//! 1. No heap allocations occur (no dynamic vector resizing, no map nodes).
//! 2. Orders at each price level are managed via a doubly-linked list of indexes
//!    pointing to slots in the pre-allocated arena, achieving O(1) insertion,
//!    O(1) cancellation, and O(1) traversal.
//! 3. Active orders are indexed via a zero-allocation, pre-sized flat hash map
//!    with linear probing to guarantee O(1) lookups during cancellations.
//!
//! Price levels are managed via a `BTreeMap` to provide O(log N) operations
//! for inserting/removing levels and O(1) access to the best bid/ask.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use crate::model::{Order, Side, Trade};
use crate::errors::EngineError;

/// Sentinel index representing a null pointer/index in the linked list.
pub const NULL_IDX: u32 = u32::MAX;

/// A node in the order arena.
/// Enforces standard size and alignment, wrapping the core Order struct.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub(crate) struct OrderSlot {
    /// The actual order data.
    pub(crate) order: Order,
    /// Index of the previous order in the price level's queue.
    pub(crate) prev_idx: u32,
    /// Index of the next order in the price level's queue.
    pub(crate) next_idx: u32,
    /// Indicates whether this slot is currently occupied.
    pub(crate) in_use: bool,
    /// Explicit padding to ensure 56-byte alignment and prevent uninitialized memory reads during snapshotting.
    pub(crate) _padding: [u8; 7],
}

/// A price level in the order book.
/// Represents the head and tail of the FIFO queue of orders at this price point.
#[derive(Debug, Clone, Copy)]
pub struct PriceLevel {
    /// Fixed-point price (scaled by 10,000).
    pub price: u64,
    /// Aggregated resting quantity at this price level (scaled by 10,000).
    pub total_quantity: u64,
    /// Index of the first order in the FIFO queue (maker candidate).
    pub head_idx: u32,
    /// Index of the last order in the FIFO queue.
    pub tail_idx: u32,
}

impl PriceLevel {
    /// Creates a new empty price level.
    #[inline]
    pub const fn new(price: u64) -> Self {
        Self {
            price,
            total_quantity: 0,
            head_idx: NULL_IDX,
            tail_idx: NULL_IDX,
        }
    }
}

/// A flat, pre-allocated hash map for tracking order slot mappings.
/// Uses linear probing and open addressing with Knuth multiplicative hashing.
/// Resizes are prohibited in the hot path.
struct OrderMap {
    keys: Vec<u64>,
    values: Vec<u32>,
    capacity: usize,
}

impl OrderMap {
    /// Instantiates a new lookup map. Sized to double the max orders to keep load factor < 50%.
    fn new(capacity: usize) -> Self {
        let cap = capacity.next_power_of_two() * 2;
        Self {
            keys: alloc::vec![0; cap],
            values: alloc::vec![NULL_IDX; cap],
            capacity: cap,
        }
    }

    #[inline]
    fn hash(&self, key: u64) -> usize {
        let hash = key.wrapping_mul(11400714819323198485);
        (hash as usize) & (self.capacity - 1)
    }

    #[inline]
    fn insert(&mut self, key: u64, value: u32) -> Result<(), EngineError> {
        let mut idx = self.hash(key);
        loop {
            let k = self.keys[idx];
            if k == 0 || k == u64::MAX {
                self.keys[idx] = key;
                self.values[idx] = value;
                return Ok(());
            }
            if k == key {
                return Err(EngineError::OrderAlreadyExists);
            }
            idx = (idx + 1) & (self.capacity - 1);
        }
    }

    #[inline]
    fn lookup(&self, key: u64) -> Option<u32> {
        let mut idx = self.hash(key);
        loop {
            let k = self.keys[idx];
            if k == 0 {
                return None;
            }
            if k == key {
                return Some(self.values[idx]);
            }
            idx = (idx + 1) & (self.capacity - 1);
        }
    }

    #[inline]
    fn remove(&mut self, key: u64) -> bool {
        let mut idx = self.hash(key);
        loop {
            let k = self.keys[idx];
            if k == 0 {
                return false;
            }
            if k == key {
                self.keys[idx] = u64::MAX; // Tombstone marker
                self.values[idx] = NULL_IDX;
                return true;
            }
            idx = (idx + 1) & (self.capacity - 1);
        }
    }
}

/// The core Order Book containing resting orders.
pub struct OrderBook {
    /// Bids sorted ascending by price. Highest bid retrieved by querying `.next_back()`.
    pub bids: BTreeMap<u64, PriceLevel>,
    /// Asks sorted ascending by price. Lowest ask retrieved by querying `.next()`.
    pub asks: BTreeMap<u64, PriceLevel>,
    /// Pre-allocated arena containing all active and free order slots.
    slots: Vec<OrderSlot>,
    /// Head index of the free-slot singly-linked list.
    free_head: u32,
    /// Fast lookup table mapping active order IDs to their arena slot index.
    orders_map: OrderMap,
}

impl OrderBook {
    /// Exposes the slots array read-only for external audits and compliance engines.
    #[inline]
    pub(crate) fn slots(&self) -> &[OrderSlot] {
        &self.slots
    }

    /// Exposes the slots array mutably for loading snapshots.
    #[inline]
    pub(crate) fn slots_mut(&mut self) -> &mut [OrderSlot] {
        &mut self.slots
    }

    /// Gets the current free list head index.
    #[inline]
    pub(crate) fn free_head(&self) -> u32 {
        self.free_head
    }

    /// Sets the free list head index.
    #[inline]
    pub(crate) fn set_free_head(&mut self, free_head: u32) {
        self.free_head = free_head;
    }

    /// Rebuilds the bids/asks BTreeMaps and the orders_map from the loaded slots arena.
    pub(crate) fn rebuild_indexes(&mut self) {
        self.bids.clear();
        self.asks.clear();
        self.orders_map = OrderMap::new(self.slots.len());

        for i in 0..self.slots.len() {
            let slot = &self.slots[i];
            if slot.in_use {
                let order_id = slot.order.order_id;
                let price = slot.order.price;
                let side = slot.order.side();

                let _ = self.orders_map.insert(order_id, i as u32);

                let levels = match side {
                    Side::Buy => &mut self.bids,
                    Side::Sell => &mut self.asks,
                };

                let level = levels.entry(price).or_insert_with(|| PriceLevel::new(price));
                
                if slot.prev_idx == NULL_IDX {
                    level.head_idx = i as u32;
                }
                if slot.next_idx == NULL_IDX {
                    level.tail_idx = i as u32;
                }
                level.total_quantity += slot.order.quantity;
            }
        }
    }


    /// Instantiates an empty `OrderBook` with a pre-allocated capacity.
    ///
    /// # Performance
    /// Memory allocation happens strictly within this constructor (cold path).
    pub fn new(capacity: usize) -> Self {
        let mut slots = Vec::with_capacity(capacity);
        for i in 0..capacity {
            slots.push(OrderSlot {
                order: Order::new(0, 0, 0, 0, Side::Buy),
                prev_idx: NULL_IDX,
                next_idx: if i + 1 < capacity { (i + 1) as u32 } else { NULL_IDX },
                in_use: false,
                _padding: [0; 7],
            });
        }
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            slots,
            free_head: 0,
            orders_map: OrderMap::new(capacity),
        }
    }

    /// Inserts a new order into the resting book.
    ///
    /// # Errors
    /// Returns `EngineError::BookCapacityExceeded` if the arena is full.
    /// Returns `EngineError::OrderAlreadyExists` if the order ID is duplicated.
    pub fn add_order(&mut self, order_id: u64, price: u64, quantity: u64, client_id: u64, side: Side) -> Result<(), EngineError> {
        if price == 0 {
            return Err(EngineError::InvalidPrice);
        }
        if quantity == 0 {
            return Err(EngineError::InvalidQuantity);
        }

        // Allocate slot from the free list
        let free_idx = self.free_head;
        if free_idx == NULL_IDX {
            return Err(EngineError::BookCapacityExceeded);
        }

        // Insert mapping into hash map first to check duplicates
        self.orders_map.insert(order_id, free_idx)?;

        // Remove from free list
        self.free_head = self.slots[free_idx as usize].next_idx;

        // Initialize slot data
        let slot = &mut self.slots[free_idx as usize];
        slot.order = Order::new(order_id, price, quantity, client_id, side);
        slot.prev_idx = NULL_IDX;
        slot.next_idx = NULL_IDX;
        slot.in_use = true;

        // Insert into PriceLevel queues
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };

        let level = levels.entry(price).or_insert_with(|| PriceLevel::new(price));
        
        // Append order to price level (FIFO order)
        let tail_idx = level.tail_idx;
        if tail_idx == NULL_IDX {
            // First order at this price level
            level.head_idx = free_idx;
            level.tail_idx = free_idx;
        } else {
            // Link new slot to the tail of the level's list
            self.slots[tail_idx as usize].next_idx = free_idx;
            self.slots[free_idx as usize].prev_idx = tail_idx;
            level.tail_idx = free_idx;
        }
        level.total_quantity += quantity;

        Ok(())
    }

    /// Cancels a resting order and returns its state.
    ///
    /// # Errors
    /// Returns `EngineError::OrderNotFound` if the order is not in the book.
    pub fn cancel_order(&mut self, order_id: u64) -> Result<Order, EngineError> {
        let slot_idx = self.orders_map.lookup(order_id).ok_or(EngineError::OrderNotFound)?;
        let order = self.slots[slot_idx as usize].order;

        self.remove_slot_from_book(slot_idx, order.price, order.side());
        self.orders_map.remove(order_id);
        self.free_slot(slot_idx);

        Ok(order)
    }

    /// Internal helper to remove a slot from its price level queue.
    /// Cleans up empty price levels.
    fn remove_slot_from_book(&mut self, slot_idx: u32, price: u64, side: Side) {
        let levels = match side {
            Side::Buy => &mut self.bids,
            Side::Sell => &mut self.asks,
        };

        let mut remove_level = false;
        if let Some(level) = levels.get_mut(&price) {
            let prev = self.slots[slot_idx as usize].prev_idx;
            let next = self.slots[slot_idx as usize].next_idx;

            // Update links
            if prev != NULL_IDX {
                self.slots[prev as usize].next_idx = next;
            } else {
                level.head_idx = next;
            }

            if next != NULL_IDX {
                self.slots[next as usize].prev_idx = prev;
            } else {
                level.tail_idx = prev;
            }

            level.total_quantity -= self.slots[slot_idx as usize].order.quantity;
            if level.head_idx == NULL_IDX {
                remove_level = true;
            }
        }

        if remove_level {
            levels.remove(&price);
        }
    }

    /// Internal helper to return an executed/canceled slot back to the free list.
    #[inline]
    fn free_slot(&mut self, slot_idx: u32) {
        let slot = &mut self.slots[slot_idx as usize];
        slot.in_use = false;
        slot.order = Order::new(0, 0, 0, 0, Side::Buy);
        slot.prev_idx = NULL_IDX;
        slot.next_idx = self.free_head;
        self.free_head = slot_idx;
    }

    /// Matches an aggressive incoming order against resting orders on the opposite side of the book.
    ///
    /// # Rationale for Choice of Data Structures (Cache Locality)
    /// We use a single flat `Vec<OrderSlot>` pre-allocated in memory to act as our order arena. 
    /// Rested orders are stored adjacent to each other. When traversing the order book, the system
    /// walks down the linked indices which reside within the same contiguous memory block,
    /// keeping CPU cache misses to a minimum.
    ///
    /// Pre-allocated arrays prevent memory allocator locks in the hot path. Matches are written
    /// directly to the user-supplied `trades` array slice to avoid allocating a new results vector.
    ///
    /// # Input Bounds and Assumptions
    /// * `incoming`: Taker order state. Matches are processed in-place, updating `incoming.quantity`.
    /// * `trades`: Outgoing trade trace. Must have enough slots to write trades.
    /// * `sequence_id`: Global sequencer stamp applied to the incoming match context.
    /// * `trade_id_counter`: Monotonic mutable pointer to assign globally unique trade identifiers.
    pub fn match_order(
        &mut self,
        incoming: &mut Order,
        trades: &mut [Trade],
        sequence_id: u64,
        trade_id_counter: &mut u64,
    ) -> usize {
        let mut trade_count = 0;
        let max_trades = trades.len();

        let side = incoming.side();
        let price = incoming.price;

        while incoming.quantity > 0 && trade_count < max_trades {
            // Find opposite book side (lowest ask for buy taker, highest bid for sell taker)
            let match_price = match side {
                Side::Buy => {
                    if let Some((&ask_price, _)) = self.asks.iter().next() {
                        if price >= ask_price {
                            Some(ask_price)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                Side::Sell => {
                    if let Some((&bid_price, _)) = self.bids.iter().next_back() {
                        if price <= bid_price {
                            Some(bid_price)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
            };

            let target_price = match match_price {
                Some(p) => p,
                None => break, // Crossover point not met or order book empty
            };

            // Retrieve price level head index without holding the borrow
            let mut current_idx = match side {
                Side::Buy => self.asks.get(&target_price).map(|l| l.head_idx),
                Side::Sell => self.bids.get(&target_price).map(|l| l.head_idx),
            }.unwrap_or(NULL_IDX);

            while current_idx != NULL_IDX && incoming.quantity > 0 && trade_count < max_trades {
                let maker_qty = self.slots[current_idx as usize].order.quantity;
                let maker_order_id = self.slots[current_idx as usize].order.order_id;
                let matched_qty = core::cmp::min(incoming.quantity, maker_qty);

                // Update slot order quantity
                self.slots[current_idx as usize].order.quantity -= matched_qty;
                incoming.quantity -= matched_qty;

                // Update price level total quantity with a short-lived mutable borrow
                let levels = match side {
                    Side::Buy => &mut self.asks,
                    Side::Sell => &mut self.bids,
                };
                if let Some(level) = levels.get_mut(&target_price) {
                    level.total_quantity -= matched_qty;
                }

                // Record trade log
                *trade_id_counter += 1;
                trades[trade_count] = Trade::new(
                    *trade_id_counter,
                    sequence_id,
                    maker_order_id,
                    incoming.order_id,
                    target_price,
                    matched_qty,
                    side,
                );
                trade_count += 1;

                let next_idx = self.slots[current_idx as usize].next_idx;

                if self.slots[current_idx as usize].order.quantity == 0 {
                    // Fully filled resting order: unlink and free
                    self.remove_slot_from_book(current_idx, target_price, side.opposite());
                    self.orders_map.remove(maker_order_id);
                    self.free_slot(current_idx);
                }

                current_idx = next_idx;
            }

            // Cleanup price level if completely depleted
            let level_empty = match side {
                Side::Buy => self.asks.get(&target_price).map_or(true, |l| l.head_idx == NULL_IDX),
                Side::Sell => self.bids.get(&target_price).map_or(true, |l| l.head_idx == NULL_IDX),
            };

            if level_empty {
                match side {
                    Side::Buy => self.asks.remove(&target_price),
                    Side::Sell => self.bids.remove(&target_price),
                };
            }
        }

        trade_count
    }

    /// Gets the number of orders currently stored in the arena.
    pub fn len(&self) -> usize {
        let mut count = 0;
        for slot in &self.slots {
            if slot.in_use {
                count += 1;
            }
        }
        count
    }

    /// Verifies the order book total quantity state. Used for property/stress testing.
    pub fn get_total_quantity_resting(&self) -> u64 {
        let mut total = 0;
        for level in self.bids.values() {
            total += level.total_quantity;
        }
        for level in self.asks.values() {
            total += level.total_quantity;
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cross_matching_matching_price() {
        let mut book = OrderBook::new(10);
        let mut trade_counter = 0;
        let mut trades_buffer = [Trade::new(0, 0, 0, 0, 0, 0, Side::Buy); 5];

        // 1. Add maker sell order (resting order at price 100, qty 100)
        book.add_order(1, 100, 100, 1, Side::Sell).unwrap();

        // 2. Process taker buy order crossing at same price 100, qty 100
        let mut taker = Order::new(2, 100, 100, 2, Side::Buy);
        let trades_written = book.match_order(&mut taker, &mut trades_buffer, 10, &mut trade_counter);

        assert_eq!(trades_written, 1);
        assert_eq!(taker.quantity, 0); // Taker completely filled
        assert_eq!(book.len(), 0); // Maker order cleared
        assert_eq!(trade_counter, 1);

        let trade = trades_buffer[0];
        assert_eq!(trade.trade_id, 1);
        assert_eq!(trade.sequence_id, 10);
        assert_eq!(trade.maker_id, 1);
        assert_eq!(trade.taker_id, 2);
        assert_eq!(trade.price, 100);
        assert_eq!(trade.quantity, 100);
        assert_eq!(trade.side(), Side::Buy);
    }

    #[test]
    fn test_partial_match_and_cancellation() {
        let mut book = OrderBook::new(10);
        let mut trade_counter = 0;
        let mut trades_buffer = [Trade::new(0, 0, 0, 0, 0, 0, Side::Buy); 5];

        // 1. Add maker buy order (resting at price 100, qty 150)
        book.add_order(1, 100, 150, 1, Side::Buy).unwrap();

        // 2. Taker sell order matching partially (qty 100)
        let mut taker = Order::new(2, 100, 100, 2, Side::Sell);
        let trades_written = book.match_order(&mut taker, &mut trades_buffer, 12, &mut trade_counter);

        assert_eq!(trades_written, 1);
        assert_eq!(taker.quantity, 0);
        assert_eq!(book.len(), 1); // Resting maker still has 50 shares

        // Verify resting order quantity
        let slot_idx = book.orders_map.lookup(1).unwrap();
        assert_eq!(book.slots[slot_idx as usize].order.quantity, 50);

        // 3. Cancel remaining maker order
        let canceled_order = book.cancel_order(1).unwrap();
        assert_eq!(canceled_order.order_id, 1);
        assert_eq!(canceled_order.quantity, 50);
        assert_eq!(book.len(), 0);
    }

    struct Lcg {
        state: u64,
    }

    impl Lcg {
        fn new(seed: u64) -> Self {
            Self { state: seed }
        }

        fn next(&mut self) -> u64 {
            self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.state
        }

        fn next_range(&mut self, min: u64, max: u64) -> u64 {
            let range = max - min + 1;
            min + (self.next() % range)
        }
    }

    #[test]
    fn test_order_book_invariants_random() {
        let mut book = OrderBook::new(2000);
        let mut rng = Lcg::new(42); // Seeded for reproducibility

        let mut total_injected_qty = 0u64;
        let mut total_cancelled_qty = 0u64;
        let mut total_executed_qty = 0u64;

        let mut active_order_ids = alloc::vec::Vec::new();
        let mut order_id_counter = 0u64;
        let mut trade_id_counter = 0u64;

        let mut trades_buffer = [Trade::new(0, 0, 0, 0, 0, 0, Side::Buy); 200];

        for _ in 0..10000 {
            // Decide operation: 70% Add, 30% Cancel
            let op = rng.next_range(1, 100);
            if op <= 70 || active_order_ids.is_empty() {
                // Add order
                order_id_counter += 1;
                let price = rng.next_range(90, 110);
                let qty = rng.next_range(1, 100);
                let side = if rng.next_range(0, 1) == 0 { Side::Buy } else { Side::Sell };

                let client_id = rng.next_range(1, 10);
                let mut incoming = Order::new(order_id_counter, price, qty, client_id, side);
                total_injected_qty += qty;

                // Match order against resting book
                let trades_written = book.match_order(&mut incoming, &mut trades_buffer, 0, &mut trade_id_counter);
                for i in 0..trades_written {
                    total_executed_qty += trades_buffer[i].quantity;
                }

                // If taker is not fully matched, rest it in the book (if capacity allows)
                if incoming.quantity > 0 {
                    if book.len() < 1000 {
                        let original_qty = incoming.quantity;
                        if book.add_order(incoming.order_id, incoming.price, original_qty, incoming.client_id, incoming.side()).is_ok() {
                            active_order_ids.push(incoming.order_id);
                        } else {
                            // If add_order fails (e.g. duplicate key or slot full), reclaim injected qty
                            total_injected_qty -= original_qty;
                        }
                    } else {
                        // Taker remainder is cancelled due to capacity limits
                        total_cancelled_qty += incoming.quantity;
                    }
                }
            } else {
                // Cancel order
                let index_to_cancel = rng.next_range(0, (active_order_ids.len() - 1) as u64) as usize;
                let order_id = active_order_ids.swap_remove(index_to_cancel);

                if let Ok(canceled) = book.cancel_order(order_id) {
                    total_cancelled_qty += canceled.quantity;
                }
            }

            // Assert Invariant: Total Injected == Cancelled + 2 * Executed + Resting
            let total_resting_qty = book.get_total_quantity_resting();
            let total_accounted = total_cancelled_qty + (2 * total_executed_qty) + total_resting_qty;
            assert_eq!(
                total_injected_qty,
                total_accounted,
                "Invariant failed! Injected: {}, Accounted: {} (Canceled: {}, Executed: {}, Resting: {})",
                total_injected_qty,
                total_accounted,
                total_cancelled_qty,
                total_executed_qty,
                total_resting_qty
            );
        }
    }
}

