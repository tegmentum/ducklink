//! ISIN (ISO 6166) algorithm, free of any WIT/host types so it can back BOTH
//! deployment paths: the `isin-component` (dynamic, WIT-dispatched) and the
//! embedded path (compiled into the core component and registered via DuckDB's
//! native scalar API, with no WIT boundary). The Duckvalue glue lives in each
//! caller; this crate is just the pure logic.

/// Strip whitespace + hyphens and upper-case.
fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Expand each letter to its 2-digit value (A=10..Z=35) and each digit to
/// itself, concatenated. Returns None on any non-alphanumeric char.
fn expand(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if c.is_ascii_digit() {
            out.push(c);
        } else if c.is_ascii_alphabetic() {
            let v = (c.to_ascii_uppercase() as u32) - ('A' as u32) + 10;
            out.push_str(&format!("{v}"));
        } else {
            return None;
        }
    }
    Some(out)
}

/// Luhn check digit (0..9) over a digit-only string.
fn luhn_check_digit(s: &str) -> Option<u32> {
    let mut sum = 0u32;
    let mut alt = true;
    for c in s.chars().rev() {
        let d = c.to_digit(10)?;
        let v = if alt {
            let x = d * 2;
            if x > 9 {
                x - 9
            } else {
                x
            }
        } else {
            d
        };
        sum += v;
        alt = !alt;
    }
    Some((10 - (sum % 10)) % 10)
}

fn expected_check_digit(normalized: &str) -> Option<u32> {
    if normalized.len() != 12 {
        return None;
    }
    expand(&normalized[..11]).as_deref().and_then(luhn_check_digit)
}

/// True if `s` (after normalization) is a valid 12-char ISIN with a correct
/// Luhn check digit.
pub fn validate(s: &str) -> bool {
    let normalized = normalize(s);
    if normalized.len() != 12 {
        return false;
    }
    let last = match normalized.as_bytes()[11] {
        b @ b'0'..=b'9' => (b - b'0') as u32,
        _ => return false,
    };
    matches!(expected_check_digit(&normalized), Some(expected) if expected == last)
}

/// The expected ISIN check digit for the first 11 characters of `s`, or None if
/// `s` is not a valid 11/12-char ISIN body.
pub fn check_digit(s: &str) -> Option<i64> {
    let normalized = normalize(s);
    let body = if normalized.len() == 12 {
        &normalized[..11]
    } else if normalized.len() == 11 {
        &normalized[..]
    } else {
        return None;
    };
    expand(body).as_deref().and_then(luhn_check_digit).map(|d| d as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn apple_valid() {
        assert!(validate("US0378331005"));
        assert!(!validate("US0378331006"));
        assert_eq!(check_digit("US037833100"), Some(5));
    }
}
