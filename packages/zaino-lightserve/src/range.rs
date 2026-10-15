//! Walking a requested block range in bounded steps.
//!
//! A range read is served as a stream, step by step, rather than composed whole
//! before the first block goes out. One step reads at most [`STEP`] heights, so
//! an in-flight range holds at most one step in memory, and the stream yields
//! to the runtime between steps.
//!
//! lightwalletd serves a range whose start is above its end in descending
//! order, so the steps run in the requested direction:
//!
//! ```text
//! steps([lo, hi])  = [lo, lo+STEP-1], [lo+STEP, lo+2·STEP-1], …, [·, hi]
//! steps([hi ↓ lo]) = reverse(steps([lo, hi])), each step's blocks reversed
//! ```

use zaino_primitives::types::{Height, HeightRange};

/// How many heights one step reads.
const STEP: u32 = 100;

/// The order a requested range is served in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    /// `start <= end`: lowest height first.
    Ascending,
    /// `start > end`: highest height first.
    Descending,
}

impl Direction {
    /// Put one step's blocks, read lowest first, into delivery order.
    pub(crate) fn order<T>(self, mut step: Vec<T>) -> Vec<T> {
        if self == Direction::Descending {
            step.reverse();
        }
        step
    }
}

/// The steps `requested` is read in, in delivery order, with the direction
/// each step's blocks are delivered in. Each step is an ascending
/// [`HeightRange`].
pub(crate) fn steps(requested: HeightRange) -> (Direction, Vec<HeightRange>) {
    let (direction, low, high) = if requested.start <= requested.end {
        (Direction::Ascending, requested.start, requested.end)
    } else {
        (Direction::Descending, requested.end, requested.start)
    };
    let mut steps = Vec::new();
    let mut step_low = u32::from(low);
    let high = u32::from(high);
    loop {
        let step_high = step_low.saturating_add(STEP - 1).min(high);
        // Both bounds lie within `[low, high]`, two valid heights, so each is
        // a valid height too.
        if let (Ok(start), Ok(end)) = (Height::try_from(step_low), Height::try_from(step_high)) {
            steps.push(HeightRange { start, end });
        }
        match step_high.checked_add(1) {
            Some(next) if step_high < high => step_low = next,
            _ => break,
        }
    }
    if direction == Direction::Descending {
        steps.reverse();
    }
    (direction, steps)
}

#[cfg(test)]
mod tests {
    use super::{steps, Direction, STEP};
    use zaino_primitives::types::{Height, HeightRange};

    fn h(height: u32) -> Height {
        Height::try_from(height).expect("a valid test height")
    }

    fn range(start: u32, end: u32) -> HeightRange {
        HeightRange {
            start: h(start),
            end: h(end),
        }
    }

    /// An ascending range splits into consecutive steps that cover it exactly.
    #[test]
    fn an_ascending_range_is_covered_in_order() {
        let (direction, steps) = steps(range(10, 10 + 2 * STEP + 4));
        assert_eq!(direction, Direction::Ascending);
        assert_eq!(
            steps,
            vec![
                range(10, 10 + STEP - 1),
                range(10 + STEP, 10 + 2 * STEP - 1),
                range(10 + 2 * STEP, 10 + 2 * STEP + 4),
            ]
        );
    }

    /// A one-height range is one step.
    #[test]
    fn a_single_height_is_one_step() {
        assert_eq!(
            steps(range(7, 7)),
            (Direction::Ascending, vec![range(7, 7)])
        );
    }

    /// A reversed range is read highest step first, and its blocks are
    /// delivered highest first.
    #[test]
    fn a_reversed_range_is_served_descending() {
        let (direction, steps) = steps(range(10 + STEP + 1, 10));
        assert_eq!(direction, Direction::Descending);
        assert_eq!(
            steps,
            vec![range(10 + STEP, 10 + STEP + 1), range(10, 10 + STEP - 1)]
        );
        assert_eq!(direction.order(vec![1, 2, 3]), vec![3, 2, 1]);
    }
}
