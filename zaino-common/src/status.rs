//! Service status types.
//!
//! This module provides [`StatusType`], an enum representing service operational states.

use std::fmt;

/// Status of a service component.
///
/// Represents the operational state of a component.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum StatusType {
    /// Running initial startup routine.
    Spawning = 0,
    /// Back-end process is currently syncing.
    Syncing = 1,
    /// Process is ready.
    Ready = 2,
    /// Process is busy working.
    Busy = 3,
    /// Running shutdown routine.
    Closing = 4,
    /// Offline.
    Offline = 5,
    /// Non-critical errors.
    RecoverableError = 6,
    /// Critical errors.
    CriticalError = 7,
}

impl From<usize> for StatusType {
    fn from(value: usize) -> Self {
        match value {
            0 => StatusType::Spawning,
            1 => StatusType::Syncing,
            2 => StatusType::Ready,
            3 => StatusType::Busy,
            4 => StatusType::Closing,
            5 => StatusType::Offline,
            6 => StatusType::RecoverableError,
            _ => StatusType::CriticalError,
        }
    }
}

impl From<StatusType> for usize {
    fn from(status: StatusType) -> Self {
        status as usize
    }
}

impl fmt::Display for StatusType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let status_str = match self {
            StatusType::Spawning => "Spawning",
            StatusType::Syncing => "Syncing",
            StatusType::Ready => "Ready",
            StatusType::Busy => "Busy",
            StatusType::Closing => "Closing",
            StatusType::Offline => "Offline",
            StatusType::RecoverableError => "RecoverableError",
            StatusType::CriticalError => "CriticalError",
        };
        write!(f, "{status_str}")
    }
}

impl StatusType {
    /// Returns the corresponding status symbol for the StatusType.
    pub fn get_status_symbol(&self) -> String {
        let (symbol, color_code) = match self {
            // Yellow Statuses
            StatusType::Syncing => ("\u{1F7E1}", "\x1b[33m"),
            // Cyan Statuses
            StatusType::Spawning | StatusType::Busy => ("\u{1F7E1}", "\x1b[36m"),
            // Green Status
            StatusType::Ready => ("\u{1F7E2}", "\x1b[32m"),
            // Grey Statuses
            StatusType::Closing | StatusType::Offline => ("\u{26AB}", "\x1b[90m"),
            // Red Error Statuses
            StatusType::RecoverableError | StatusType::CriticalError => ("\u{1F534}", "\x1b[31m"),
        };

        format!("{}{}{}", color_code, symbol, "\x1b[0m")
    }

    /// Look at two statuses, and return the more 'severe' of the two.
    pub fn combine(self, other: StatusType) -> StatusType {
        match (self, other) {
            // If either is Closing, return Closing.
            (StatusType::Closing, _) | (_, StatusType::Closing) => StatusType::Closing,
            // If either is Offline or CriticalError, return CriticalError.
            (StatusType::Offline, _)
            | (_, StatusType::Offline)
            | (StatusType::CriticalError, _)
            | (_, StatusType::CriticalError) => StatusType::CriticalError,
            // If either is RecoverableError, return RecoverableError.
            (StatusType::RecoverableError, _) | (_, StatusType::RecoverableError) => {
                StatusType::RecoverableError
            }
            // If either is Spawning, return Spawning.
            (StatusType::Spawning, _) | (_, StatusType::Spawning) => StatusType::Spawning,
            // If either is Syncing, return Syncing.
            (StatusType::Syncing, _) | (_, StatusType::Syncing) => StatusType::Syncing,
            // Otherwise, return Ready.
            _ => StatusType::Ready,
        }
    }
}
