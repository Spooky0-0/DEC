//! O(1) Binary State Snapshotter for fast startup recovery.
//!
//! Provides ultra-low-latency state preservation by casting the contiguous
//! pre-allocated OrderBook slots arena directly to raw bytes for sequential disk write.
//! Recovery loads this snapshot and reconstructs the active index tree in O(K) where
//! K is the active resting order count, avoiding full transaction replay.

#![cfg(feature = "std")]

use std::fs::File;
use std::io::{Write, Read};
use crate::book::OrderBook;


/// Serializes the OrderBook state to a compact binary snapshot file.
///
/// # Safety and Performance
/// This function casts the underlying `slots` arena of the `OrderBook` directly
/// to a raw byte slice using `std::slice::from_raw_parts` to achieve true O(1) serialization.
///
/// # Assumptions
/// * **Endianness**: Assumes that the platform saving the snapshot and the platform
///   loading the snapshot use the same byte-endianness (e.g. little-endian x86_64).
/// * **Struct Layout**: Assumes the target binary runs the same compiler layout representation (`#[repr(C)]`).
pub fn save_snapshot(book: &OrderBook, path: &str) -> Result<(), std::io::Error> {
    let mut file = File::create(path)?;

    // 1. Write metadata header: free_head (4 bytes) and slots length (8 bytes)
    let slots_slice = book.slots();
    let slots_len = slots_slice.len() as u64;
    
    // We get book's free_head. Wait, free_head is private to book.
    // Let's ensure we expose free_head as a pub(crate) method on OrderBook.
    let free_head = book.free_head();

    file.write_all(&free_head.to_le_bytes())?;
    file.write_all(&slots_len.to_le_bytes())?;

    // 2. Cast and write the contiguous slots arena in O(1)
    let ptr = slots_slice.as_ptr() as *const u8;
    let len = slots_slice.len() * core::mem::size_of::<crate::book::OrderSlot>();
    
    let byte_slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    file.write_all(byte_slice)?;

    file.sync_all()?;
    Ok(())
}

/// Restores the OrderBook state from a binary snapshot file and reconstructs price indexes.
///
/// # Safety
/// Reads raw binary bytes directly into the pre-allocated slots arena.
pub fn load_snapshot(book: &mut OrderBook, path: &str) -> Result<(), std::io::Error> {
    let mut file = File::open(path)?;

    // 1. Read metadata header
    let mut meta_buf = [0u8; 12];
    file.read_exact(&mut meta_buf)?;
    
    let free_head = u32::from_le_bytes(meta_buf[0..4].try_into().unwrap());
    let slots_len = u64::from_le_bytes(meta_buf[4..12].try_into().unwrap()) as usize;

    // 2. Read raw binary slots data directly into the pre-allocated Vec
    // Safety: Sizing check to prevent buffer overflow.
    if slots_len != book.slots().len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Snapshot slots length mismatch",
        ));
    }

    // Access mutable slice of book slots.
    // Let's add a mutable slots accessor pub(crate) fn slots_mut(&mut self) to OrderBook.
    let slots_slice_mut = book.slots_mut();
    let ptr = slots_slice_mut.as_mut_ptr() as *mut u8;
    let len = slots_slice_mut.len() * core::mem::size_of::<crate::book::OrderSlot>();

    let byte_slice_mut = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
    file.read_exact(byte_slice_mut)?;

    // Update book metadata
    book.set_free_head(free_head);

    // 3. Rebuild BTreeMap price indexes and lookup OrderMap
    book.rebuild_indexes();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Side;


    #[test]
    fn test_binary_state_snapshot_recovery() {
        let path = "test_snapshot.bin";
        let mut book_original = OrderBook::new(100);

        // 1. Inject some orders
        book_original.add_order(1, 100, 50, 10, Side::Buy).unwrap();
        book_original.add_order(2, 105, 30, 20, Side::Sell).unwrap();
        book_original.add_order(3, 100, 25, 30, Side::Buy).unwrap();

        // 2. Save snapshot to disk
        save_snapshot(&book_original, path).unwrap();

        // 3. Load snapshot into a clean OrderBook instance
        let mut book_restored = OrderBook::new(100);
        load_snapshot(&mut book_restored, path).unwrap();

        // Cleanup test file
        let _ = std::fs::remove_file(path);

        // 4. Verify book structure parity
        assert_eq!(book_restored.free_head(), book_original.free_head());
        assert_eq!(book_restored.len(), 3);
        assert_eq!(book_restored.get_total_quantity_resting(), 105);

        // Verify active keys and structures in PriceLevels
        assert!(book_restored.bids.contains_key(&100));
        assert!(book_restored.asks.contains_key(&105));

        let restored_bid_level = book_restored.bids.get(&100).unwrap();
        let original_bid_level = book_original.bids.get(&100).unwrap();
        assert_eq!(restored_bid_level.total_quantity, 75);
        assert_eq!(restored_bid_level.head_idx, original_bid_level.head_idx);
        assert_eq!(restored_bid_level.tail_idx, original_bid_level.tail_idx);
    }
}

