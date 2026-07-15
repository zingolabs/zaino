//! SelfCumulative x Fold: running maximum where extraction depends on prior state.
//!
//! Each block contributes a value. If the prior max exceeds a threshold,
//! the contribution is halved (simulating diminishing returns). The fold
//! tracks the running maximum.

use crate::descriptor::{Fold, SelfCumulative};
use crate::encode::{Decode, DecodeError, Encode};
use crate::primitives::IndexId;
use crate::traits::{
    ExtractCumulative, ExtractError, IndexDef, MergeFold, Schema, SchemaDecodeError,
};

/// Block context: the raw value for this block.
pub struct Context {
    /// Arbitrary value carried by this block.
    pub value: u32,
}

/// The running maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunningMax(u64);

impl RunningMax {
    /// Create a running max.
    pub const fn new(max: u64) -> Self {
        Self(max)
    }

    /// The raw value.
    pub const fn value(&self) -> u64 {
        self.0
    }
}

impl Encode for RunningMax {
    fn encode(&self) -> Vec<u8> {
        self.0.to_le_bytes().to_vec()
    }
}

impl Decode for RunningMax {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        Ok(Self(u64::decode(bytes)?))
    }
}

/// Unit key type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxKey;

impl Encode for MaxKey {
    fn encode(&self) -> Vec<u8> {
        b"max".to_vec()
    }
}

impl Decode for MaxKey {
    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        if bytes == b"max" {
            Ok(Self)
        } else {
            Err(DecodeError::Failed("expected 'max' key".into()))
        }
    }
}

/// Cumulative maximum with diminishing returns.
pub struct CumulativeMaxIndex;

/// Index identity.
pub const ID: IndexId = IndexId::new("cumulative_max");

/// Prior max above this value causes contributions to be halved.
const HALVING_THRESHOLD: u64 = 20;

impl IndexDef for CumulativeMaxIndex {
    type Scope = SelfCumulative;
    type Composition = Fold;
    type Delta = u64;
    type BlockContext = Context;

    const NAME: IndexId = ID;
}

impl ExtractCumulative for CumulativeMaxIndex {
    type PriorState = RunningMax;

    fn extract(ctx: &Context, prior: &RunningMax) -> Result<u64, ExtractError> {
        let base = u64::from(ctx.value);
        if prior.value() > HALVING_THRESHOLD {
            Ok(base / 2)
        } else {
            Ok(base)
        }
    }
}

impl MergeFold for CumulativeMaxIndex {
    type FoldState = RunningMax;

    fn initial_state() -> RunningMax {
        RunningMax::new(0)
    }

    fn fold(state: &mut RunningMax, delta: u64) {
        if delta > state.0 {
            state.0 = delta;
        }
    }
}

impl Schema<RunningMax> for CumulativeMaxIndex {
    type Key = MaxKey;
    type Value = RunningMax;

    fn into_entries(max: RunningMax) -> Vec<(Self::Key, Self::Value)> {
        vec![(MaxKey, max)]
    }

    fn from_entries(entries: Vec<(Self::Key, Self::Value)>) -> RunningMax {
        entries
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .unwrap_or(RunningMax::new(0))
    }

    fn encode_key(key: &Self::Key) -> Vec<u8> { key.encode() }
    fn encode_value(value: &Self::Value) -> Vec<u8> { value.encode() }
    fn decode_key(bytes: &[u8]) -> Result<Self::Key, SchemaDecodeError> {
        MaxKey::decode(bytes).map_err(|e| SchemaDecodeError::Invalid(e.to_string()))
    }
    fn decode_value(bytes: &[u8]) -> Result<Self::Value, SchemaDecodeError> {
        RunningMax::decode(bytes).map_err(|e| SchemaDecodeError::Invalid(e.to_string()))
    }
}
