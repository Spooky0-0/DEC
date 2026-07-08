//! Compliance Guard checking for wash trading patterns.
//!
//! Intercepts incoming orders prior to matching engine execution.
//! If an incoming order crosses with any resting order on the opposite side of the
//! book that belongs to the same client, the order is flagged and rejected to prevent wash trading.

use crate::model::{Order, Side};
use crate::book::{OrderBook, NULL_IDX};
use crate::errors::EngineError;

/// Inspects an incoming order against the resting order book to verify compliance.
///
/// # Errors
/// Returns `Err(EngineError::WashTradingViolation)` if a wash-trading pattern is detected
/// (i.e. if the order would cross and match against another order from the same client).
pub fn check_order(order: &Order, book: &OrderBook) -> Result<(), EngineError> {
    let side = order.side();
    let price = order.price;
    let client_id = order.client_id;

    match side {
        Side::Buy => {
            // Incoming Buy order matches against resting Sell orders (asks)
            // Traverse asks starting from the lowest price upwards
            for (&ask_price, level) in &book.asks {
                if ask_price > price {
                    break; // No further crossover possible
                }
                
                let mut current_idx = level.head_idx;
                while current_idx != NULL_IDX {
                    let slot = &book.slots()[current_idx as usize];
                    if slot.order.client_id == client_id {
                        return Err(EngineError::WashTradingViolation);
                    }
                    current_idx = slot.next_idx;
                }
            }
        }
        Side::Sell => {
            // Incoming Sell order matches against resting Buy orders (bids)
            // Traverse bids starting from the highest price downwards
            for (&bid_price, level) in book.bids.iter().rev() {
                if bid_price < price {
                    break; // No further crossover possible
                }

                let mut current_idx = level.head_idx;
                while current_idx != NULL_IDX {
                    let slot = &book.slots()[current_idx as usize];
                    if slot.order.client_id == client_id {
                        return Err(EngineError::WashTradingViolation);
                    }
                    current_idx = slot.next_idx;
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wash_trading_prevention() {
        let mut book = OrderBook::new(10);
        
        // 1. Add maker resting sell order (Client ID 100, Price 10, Qty 50)
        book.add_order(1, 10, 50, 100, Side::Sell).unwrap();

        // 2. Incoming buy order from client 200 (Allowed - different client)
        let incoming_allowed = Order::new(2, 10, 50, 200, Side::Buy);
        assert!(check_order(&incoming_allowed, &book).is_ok());

        // 3. Incoming buy order from client 100 (Rejected - wash trading violation)
        let incoming_violation = Order::new(3, 10, 50, 100, Side::Buy);
        assert_eq!(
            check_order(&incoming_violation, &book),
            Err(EngineError::WashTradingViolation)
        );
    }
}
