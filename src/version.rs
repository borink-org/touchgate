//! Versions and dates, as the changelog and the manifests write them.

use std::cmp::Ordering;
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// A SemVer version without build metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    /// The pre-release identifiers, such as `rc` and `1` in `1.0.0-rc.1`.
    /// Empty for a release.
    pre: Vec<Identifier>,
}

/// A pre-release identifier. SemVer orders a numeric one below any
/// alphanumeric one, which is the order of the variants.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Identifier {
    Number(u64),
    Text(String),
}

impl Version {
    pub fn parse(text: &str) -> Result<Self, String> {
        let invalid = || format!("`{text}` is not a version such as 0.4.0 or 1.0.0-rc.1");
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let numbers: Vec<u64> = core
            .split('.')
            .map(|part| number(part).ok_or_else(invalid))
            .collect::<Result<_, _>>()?;
        let [major, minor, patch] = numbers[..] else {
            return Err(invalid());
        };
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => pre
                .split('.')
                .map(|part| {
                    if part.is_empty()
                        || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    {
                        Err(invalid())
                    } else if part.bytes().all(|b| b.is_ascii_digit()) {
                        number(part).map(Identifier::Number).ok_or_else(invalid)
                    } else {
                        Ok(Identifier::Text(part.to_owned()))
                    }
                })
                .collect::<Result<_, _>>()?,
        };
        Ok(Self {
            major,
            minor,
            patch,
            pre,
        })
    }
}

/// A decimal number without leading zeros.
fn number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if text.len() > 1 && text.starts_with('0') {
        return None;
    }
    text.parse().ok()
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let core =
            (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch));
        // A release sorts above its pre-releases.
        core.then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => self.pre.cmp(&other.pre),
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (index, identifier) in self.pre.iter().enumerate() {
            f.write_str(if index == 0 { "-" } else { "." })?;
            match identifier {
                Identifier::Number(number) => write!(f, "{number}")?,
                Identifier::Text(text) => f.write_str(text)?,
            }
        }
        Ok(())
    }
}

/// A calendar date, `YYYY-MM-DD`. The field order is the date order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    year: u16,
    month: u8,
    day: u8,
}

impl Date {
    pub fn parse(text: &str) -> Result<Self, String> {
        let invalid = || format!("`{text}` is not a date such as 2026-10-04");
        let bytes = text.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return Err(invalid());
        }
        let field = |range: std::ops::Range<usize>| {
            let part = &text[range];
            part.bytes()
                .all(|b| b.is_ascii_digit())
                .then(|| part.parse::<u16>().ok())
                .flatten()
                .ok_or_else(invalid)
        };
        let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return Err(invalid());
        }
        Ok(Self {
            year,
            month: month as u8,
            day: day as u8,
        })
    }

    /// Today in UTC.
    pub fn today() -> Self {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_secs();
        // Howard Hinnant's `civil_from_days`, for days since 1970-01-01.
        let z = (seconds / 86_400) as i64 + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        Self {
            year: year as u16,
            month: month as u8,
            day: day as u8,
        }
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_as_semver() {
        let order = [
            "0.0.3",
            "0.0.4-alpha",
            "0.0.4-alpha.1",
            "0.0.4-alpha.beta",
            "0.0.4-rc.2",
            "0.0.4-rc.10",
            "0.0.4",
            "0.1.0",
        ];
        for pair in order.windows(2) {
            let (lower, higher) = (Version::parse(pair[0]), Version::parse(pair[1]));
            assert!(
                lower.unwrap() < higher.unwrap(),
                "{} < {}",
                pair[0],
                pair[1]
            );
        }
        for invalid in ["0.0", "00.1.0", "0.0.4-", "0.0.4-rc..1", "0.0.4+build"] {
            assert!(Version::parse(invalid).is_err(), "{invalid}");
        }
    }
}
