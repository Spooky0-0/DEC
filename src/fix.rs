//! Zero-allocation FIX Protocol parser/gateway.
//!
//! Designed for high-frequency low-latency ingestion. Parses tag-value pairs
//! in-place from a raw byte buffer without heap allocation, copying, or dynamic string parsing.
//! Supports standard FIX delimiters (SOH `\x01` or readable `|`).

use crate::model::{Order, Side};
use crate::errors::EngineError;

/// Fast zero-allocation parser for tag-value FIX messages.
pub struct FixParser<'a> {
    data: &'a [u8],
    pos: usize,
    delimiter: u8,
}

impl<'a> FixParser<'a> {
    /// Creates a new parser instance over a raw byte slice.
    /// Detects whether SOH `\x01` or pipe `|` is used as the delimiter.
    pub fn new(data: &'a [u8]) -> Self {
        let delimiter = if data.contains(&1) { 1 } else { b'|' };
        Self {
            data,
            pos: 0,
            delimiter,
        }
    }

    /// Pulls the next tag-value pair from the buffer without copying or allocating.
    /// Returns `Some((tag, value_slice))` on success, `None` if EOF is reached,
    /// or `Some(Err(EngineError))` if a formatting error is encountered.
    pub fn next_tag(&mut self) -> Option<Result<(u32, &'a [u8]), EngineError>> {
        if self.pos >= self.data.len() {
            return None;
        }

        // Find '=' character for tag boundary
        let start = self.pos;
        let mut eq_pos = None;
        for i in start..self.data.len() {
            if self.data[i] == b'=' {
                eq_pos = Some(i);
                break;
            }
        }

        let eq_idx = match eq_pos {
            Some(idx) => idx,
            None => return Some(Err(EngineError::InvalidSide)), // Bad format
        };

        // Parse tag number
        let tag_slice = &self.data[start..eq_idx];
        let mut tag = 0u32;
        for &b in tag_slice {
            if b >= b'0' && b <= b'9' {
                tag = tag.wrapping_mul(10).wrapping_add((b - b'0') as u32);
            } else {
                return Some(Err(EngineError::InvalidSide));
            }
        }

        // Find delimiter character for value boundary
        let val_start = eq_idx + 1;
        let mut delim_pos = None;
        for i in val_start..self.data.len() {
            if self.data[i] == self.delimiter {
                delim_pos = Some(i);
                break;
            }
        }

        let delim_idx = match delim_pos {
            Some(idx) => idx,
            None => self.data.len(), // Fallback if trailing delimiter is missing
        };

        let val_slice = &self.data[val_start..delim_idx];
        self.pos = delim_idx + 1; // Advance past delimiter

        Some(Ok((tag, val_slice)))
    }
}

/// Helper function to parse a byte slice to u64.
/// 
/// If the string is purely numeric, performs a standard base-10 conversion.
/// If it is non-numeric, copies the first 8 bytes and casts directly to u64
/// (using little-endian conversion) to ensure zero-allocation compatibility.
#[inline]
pub fn bytes_to_u64(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    
    // Check if numeric
    let mut is_numeric = true;
    for &b in bytes {
        if b < b'0' || b > b'9' {
            is_numeric = false;
            break;
        }
    }

    if is_numeric {
        let mut val = 0u64;
        for &b in bytes {
            val = val.wrapping_mul(10).wrapping_add((b - b'0') as u64);
        }
        val
    } else {
        // Fallback: Copy up to 8 bytes and load as le u64
        let mut buf = [0u8; 8];
        let len = core::cmp::min(bytes.len(), 8);
        buf[..len].copy_from_slice(&bytes[..len]);
        u64::from_le_bytes(buf)
    }
}

/// Maps parsed FIX tags to an internal matching engine Order.
///
/// Handles `NewOrderSingle` mappings:
/// - Tag 11 (ClOrdID) -> `order_id`
/// - Tag 38 (OrderQty) -> `quantity`
/// - Tag 44 (Price) -> `price`
/// - Tag 49 (SenderCompID) -> `client_id`
/// - Tag 54 (Side: '1' = Buy, '2' = Sell) -> `side`
pub fn parse_fix_to_order(raw_message: &[u8]) -> Result<Order, EngineError> {
    let mut parser = FixParser::new(raw_message);
    
    let mut order_id = 0u64;
    let mut price = 0u64;
    let mut quantity = 0u64;
    let mut client_id = 0u64;
    let mut side = Side::Buy;

    let mut has_id = false;
    let mut has_qty = false;
    let mut has_price = false;
    let mut has_client = false;
    let mut has_side = false;

    while let Some(res) = parser.next_tag() {
        let (tag, val) = res?;
        match tag {
            11 => {
                order_id = bytes_to_u64(val);
                has_id = true;
            }
            38 => {
                quantity = bytes_to_u64(val);
                has_qty = true;
            }
            44 => {
                price = bytes_to_u64(val);
                has_price = true;
            }
            49 => {
                client_id = bytes_to_u64(val);
                has_client = true;
            }
            54 => {
                if val == b"1" {
                    side = Side::Buy;
                    has_side = true;
                } else if val == b"2" {
                    side = Side::Sell;
                    has_side = true;
                } else {
                    return Err(EngineError::InvalidSide);
                }
            }
            _ => {} // Ignore unrelated headers/fields
        }
    }

    if has_id && has_qty && has_price && has_client && has_side {
        Ok(Order::new(order_id, price, quantity, client_id, side))
    } else {
        Err(EngineError::InvalidStateTransition)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zero_allocation_fix_parser() {
        let raw = b"8=FIX.4.4|9=124|35=D|49=SENDER|56=TARGET|11=9999|54=1|38=500|44=10250|10=223|";
        let mut parser = FixParser::new(raw);

        let mut count = 0;
        while let Some(res) = parser.next_tag() {
            let (tag, val) = res.unwrap();
            match tag {
                11 => assert_eq!(val, b"9999"),
                49 => assert_eq!(val, b"SENDER"),
                54 => assert_eq!(val, b"1"),
                38 => assert_eq!(val, b"500"),
                44 => assert_eq!(val, b"10250"),
                _ => {}
            }
            count += 1;
        }
        assert_eq!(count, 10);
    }

    #[test]
    fn test_parse_fix_to_order() {
        let raw = b"8=FIX.4.4|35=D|11=9999|49=1001|54=2|38=500|44=10250|";
        let order = parse_fix_to_order(raw).unwrap();

        assert_eq!(order.order_id, 9999);
        assert_eq!(order.client_id, 1001);
        assert_eq!(order.quantity, 500);
        assert_eq!(order.price, 10250);
        assert_eq!(order.side(), Side::Sell);
    }
}
