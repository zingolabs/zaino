//! Request classes: priority, permits, hedge, round end (`traffic-balancer.md` §3)

use std::time::Duration;

/// Declaration order = priority (a freed permit goes to the first waiting class)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Class {
    Poll,
    Submit,
    TipBlock,
    Headers,
    Lookup,
    Bytes,
    BulkBlock,
}

pub(crate) const CLASSES: usize = 7;

/// Per member, one counter per class
pub(crate) type PerClass<T> = [T; CLASSES];

impl Class {
    pub(crate) const ALL: PerClass<Class> = [
        Self::Poll,
        Self::Submit,
        Self::TipBlock,
        Self::Headers,
        Self::Lookup,
        Self::Bytes,
        Self::BulkBlock,
    ];

    pub(crate) fn index(self) -> usize {
        self as usize
    }

    /// Permits no other class may take from a trusted member
    pub(crate) const fn reserve(self) -> u32 {
        match self {
            Self::Poll | Self::Submit | Self::TipBlock | Self::Lookup | Self::BulkBlock => 1,
            Self::Headers | Self::Bytes => 0,
        }
    }

    /// Ceiling per 32 connections (scaled to `max_connections`, capped at the rest)
    pub(crate) fn ceiling_per_32(self) -> u32 {
        match self {
            Self::Poll => 1,
            Self::Submit | Self::Bytes => 2,
            Self::TipBlock | Self::Headers => 4,
            Self::Lookup => 8,
            Self::BulkBlock => 32,
        }
    }

    /// `None` = never hedged; else hedge past max(this, the member's p95)
    pub(crate) fn hedge_floor(self) -> Option<Duration> {
        match self {
            Self::TipBlock => Some(Duration::from_secs(2)),
            Self::Lookup => Some(Duration::from_secs(1)),
            Self::BulkBlock => Some(Duration::from_secs(15)),
            Self::Poll | Self::Submit | Self::Headers | Self::Bytes => None,
        }
    }

    /// Checkable answers only (trusted-only = the "only source" rows, `chainview.md` §1)
    pub(crate) fn peers(self) -> bool {
        !matches!(self, Self::Poll | Self::Submit | Self::Lookup)
    }

    /// Every member tried → next round after 1 s (else unanswered)
    pub(crate) fn retries_rounds(self) -> bool {
        matches!(self, Self::TipBlock | Self::BulkBlock)
    }

    /// `CatchingUp` excluded (no mempool, lagging chain)
    pub(crate) fn needs_synced(self) -> bool {
        matches!(self, Self::Lookup | Self::Bytes)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Poll => "poll",
            Self::Submit => "submit",
            Self::TipBlock => "tip_block",
            Self::Headers => "headers",
            Self::Lookup => "lookup",
            Self::Bytes => "bytes",
            Self::BulkBlock => "bulk_block",
        }
    }
}
