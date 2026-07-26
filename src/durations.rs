use crate::error::AppError;

const MAX_SECONDS: i64 = 30 * 24 * 60 * 60;

pub fn parse(raw: &str) -> Result<i64, AppError> {
    if raw.trim().is_empty() {
        return Err(AppError::usage("empty duration"));
    }
    let s = raw.trim().to_lowercase();

    if s.bytes().all(|b| b.is_ascii_digit()) {
        let v = s.parse::<i64>().map_err(|_| too_large(raw))?;
        if v <= 0 {
            return Err(AppError::usage("duration must be positive"));
        }
        return cap(v, raw);
    }

    // Tokenize into (value, unit) pairs; units must appear in d,h,m,s order, each at most once.
    let bytes = s.as_bytes();
    let mut idx = 0;
    let mut acc = [0i64; 4]; // d, h, m, s
    let mut last_order: i32 = -1;
    let invalid = || AppError::usage(format!("invalid duration: '{raw}' (try 1h30m, 90s, etc.)"));

    while idx < bytes.len() {
        let start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == start || idx == bytes.len() {
            return Err(invalid()); // missing digits, or digits with no trailing unit
        }
        let order = match bytes[idx] {
            b'd' => 0,
            b'h' => 1,
            b'm' => 2,
            b's' => 3,
            _ => return Err(invalid()),
        };
        if order <= last_order {
            return Err(invalid()); // out of order or repeated unit
        }
        last_order = order;
        acc[order as usize] = s[start..idx].parse::<i64>().map_err(|_| too_large(raw))?;
        idx += 1;
    }

    let total = acc[0]
        .checked_mul(86_400)
        .and_then(|d| d.checked_add(acc[1].checked_mul(3_600)?))
        .and_then(|v| v.checked_add(acc[2].checked_mul(60)?))
        .and_then(|v| v.checked_add(acc[3]))
        .ok_or_else(|| too_large(raw))?;
    if total <= 0 {
        return Err(AppError::usage(format!("invalid duration: '{raw}'")));
    }
    cap(total, raw)
}

fn cap(seconds: i64, raw: &str) -> Result<i64, AppError> {
    if seconds > MAX_SECONDS {
        return Err(too_large(raw));
    }
    Ok(seconds)
}

fn too_large(raw: &str) -> AppError {
    AppError::usage(format!("duration exceeds maximum of 30d: '{raw}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_forms_and_invalid_syntax() {
        for (raw, seconds) in [
            ("3600", 3600),
            ("90s", 90),
            ("5m", 300),
            ("1h30m", 5400),
            ("2h45m30s", 9930),
            ("1d", 86_400),
            ("  1H30M  ", 5400),
        ] {
            assert_eq!(parse(raw).unwrap(), seconds, "duration={raw:?}");
        }
        for raw in [
            "", "   ", "0", "0s", "abc", "1h30", "30m1h", "1h1h", "m", "1x", "h30m", "-5",
        ] {
            assert!(parse(raw).is_err(), "duration={raw:?}");
        }
    }

    #[test]
    fn caps_at_30d() {
        assert_eq!(parse("30d").unwrap(), MAX_SECONDS);
        assert!(parse("31d").is_err());
        assert!(parse("2592001").is_err());
    }

    #[test]
    fn overflow_is_too_large() {
        assert!(parse("99999999999999999999s").is_err());
    }
}
