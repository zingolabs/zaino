//! Durations and byte sizes as log lines show them

use std::{fmt, time::Duration};

/// 3 significant figures, decimal units: `512B`, `8.10GB`, `31.0GB`, `134MB`
pub struct ByteSize(pub u64);

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
        let (mut value, mut unit) = (self.0 as f64, 0);
        // 999.5 → next unit (else 3 figures round up to a 4-digit `1000MB`)
        while value >= 999.5 && unit + 1 < UNITS.len() {
            value /= 1000.0;
            unit += 1;
        }
        match value {
            _ if unit == 0 => write!(f, "{}B", self.0),
            value if value < 9.995 => write!(f, "{value:.2}{}", UNITS[unit]),
            value if value < 99.95 => write!(f, "{value:.1}{}", UNITS[unit]),
            value => write!(f, "{value:.0}{}", UNITS[unit]),
        }
    }
}

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

    #[test]
    fn byte_sizes_keep_three_figures_in_decimal_units() {
        for (bytes, shown) in [
            (0, "0B"),
            (512, "512B"),
            (999, "999B"),
            (999_500, "1.00MB"),
            (3_100_000, "3.10MB"),
            (134_000_000, "134MB"),
            (8_100_000_000, "8.10GB"),
            (31_000_000_000, "31.0GB"),
            (u64::MAX, "18446744TB"),
        ] {
            assert_eq!(ByteSize(bytes).to_string(), shown, "{bytes}");
        }
    }
}
