//! Durations as log lines show them

use std::{fmt, time::Duration};

/// Two largest units, no spaces: `812ms`, `45s`, `9m57s`, `2h13m`, `3d04h`
pub struct Human(pub Duration);

impl fmt::Display for Human {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        if secs == 0 {
            return write!(f, "{}ms", self.0.as_millis());
        }
        let (d, h, m, s) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60, secs % 60);
        match (d, h, m) {
            (0, 0, 0) => write!(f, "{s}s"),
            (0, 0, _) => write!(f, "{m}m{s:02}s"),
            (0, _, _) => write!(f, "{h}h{m:02}m"),
            _ => write!(f, "{d}d{h:02}h"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_durations_keep_the_two_largest_units() {
        assert_eq!(Human(Duration::from_millis(812)).to_string(), "812ms");
        for (secs, shown) in [
            (0, "0ms"),
            (45, "45s"),
            (597, "9m57s"),
            (3_600, "1h00m"),
            (7_980, "2h13m"),
            (273_600, "3d04h"),
        ] {
            assert_eq!(Human(Duration::from_secs(secs)).to_string(), shown, "{secs}");
        }
    }
}
