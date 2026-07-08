//! Custom domain-specific exceptions for the Deterministic Exchange Core (DEC).
//!
//! Implemented manually to maintain `no_std` compatibility and avoid external crate dependencies.

use core::fmt;

/// Custom domain-specific exceptions for matching engine operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineError {
    /// The order price is invalid (e.g. 0).
    InvalidPrice,
    /// The order quantity is invalid (e.g. 0).
    InvalidQuantity,
    /// The order ID already exists in the book.
    OrderAlreadyExists,
    /// The order ID was not found for cancellation or modification.
    OrderNotFound,
    /// The pre-allocated order capacity was exceeded.
    BookCapacityExceeded,
    /// An invalid transition of order state was attempted.
    InvalidStateTransition,
    /// The side byte provided does not correspond to a valid side.
    InvalidSide,
    /// The order would result in a wash trade with a resting order from the same client.
    WashTradingViolation,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::InvalidPrice => write!(f, "InvalidPrice: Order price must be greater than zero"),
            EngineError::InvalidQuantity => write!(f, "InvalidQuantity: Order quantity must be greater than zero"),
            EngineError::OrderAlreadyExists => write!(f, "OrderAlreadyExists: Order ID already exists in the active book"),
            EngineError::OrderNotFound => write!(f, "OrderNotFound: Order ID was not found in the active book"),
            EngineError::BookCapacityExceeded => write!(f, "BookCapacityExceeded: Maximum pre-allocated slot capacity has been reached"),
            EngineError::InvalidStateTransition => write!(f, "InvalidStateTransition: Invalid state transition for active order"),
            EngineError::InvalidSide => write!(f, "InvalidSide: Invalid side value provided"),
            EngineError::WashTradingViolation => write!(f, "WashTradingViolation: Order rejected due to detected self-matching wash trade pattern"),
        }
    }
}

// Implement standard Error trait if std is available.
#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "std")]
impl std::error::Error for EngineError {}

