/// Fast timestamp → epoch millis parser for ISO 8601 / RFC 3339 format.
/// Handles: "2026-03-08T12:00:00Z", "2026-03-08T12:00:00.123Z"
/// Pure arithmetic, no chrono overhead (~0.1µs vs chrono ~3-5µs).
/// Falls back to chrono for timezone offsets (e.g., "+00:00") and non-standard formats.
pub fn parse_ts_to_ms(ts: &str) -> Option<i64> {
    fast_parse_ts_to_ms(ts).or_else(|| {
        chrono::DateTime::parse_from_rfc3339(ts)
            .ok()
            .map(|dt| dt.timestamp_millis())
            .filter(|&ms| ms > 0)
    })
}

/// Fast path: handles UTC timestamps ending in 'Z' with optional fractional seconds.
fn fast_parse_ts_to_ms(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    // Minimum: "2026-03-08T12:00:00Z" = 20 chars
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }

    // Fast path only handles 'Z' suffix (UTC)
    if *b.last()? != b'Z' {
        return None;
    }

    let year = parse_digits4(b, 0)? as i64;
    let month = parse_digits2(b, 5)? as u32;
    let day = parse_digits2(b, 8)? as u32;
    let hour = parse_digits2(b, 11)? as i64;
    let min = parse_digits2(b, 14)? as i64;
    let sec = parse_digits2(b, 17)? as i64;

    // Milliseconds (optional: ".123Z" after seconds)
    let ms = if b.len() > 20 && b[19] == b'.' {
        let frac_start = 20;
        let frac_end = b.len() - 1; // before 'Z'
        if frac_end > frac_start {
            // Cap at 9 fractional digits to avoid u32 overflow
            let effective_end = if frac_end - frac_start > 9 { frac_start + 9 } else { frac_end };
            let frac_str = std::str::from_utf8(&b[frac_start..effective_end]).ok()?;
            let frac: u32 = frac_str.parse().ok()?;
            let digits = (effective_end - frac_start) as u32;
            // Normalize to milliseconds
            match digits {
                1 => (frac * 100) as i64,
                2 => (frac * 10) as i64,
                3 => frac as i64,
                _ => (frac / 10u32.pow(digits - 3)) as i64,
            }
        } else {
            0
        }
    } else {
        0
    };

    // Days from epoch (simplified: no leap second handling needed for ms precision)
    let days = days_from_civil(year, month, day)?;
    let epoch_ms = days * 86_400_000 + hour * 3_600_000 + min * 60_000 + sec * 1_000 + ms;

    Some(epoch_ms)
}

#[inline]
fn parse_digits2(b: &[u8], i: usize) -> Option<u32> {
    let d1 = (b[i] as u32).wrapping_sub(b'0' as u32);
    let d2 = (b[i + 1] as u32).wrapping_sub(b'0' as u32);
    if d1 <= 9 && d2 <= 9 { Some(d1 * 10 + d2) } else { None }
}

#[inline]
fn parse_digits4(b: &[u8], i: usize) -> Option<u32> {
    let d1 = (b[i] as u32).wrapping_sub(b'0' as u32);
    let d2 = (b[i + 1] as u32).wrapping_sub(b'0' as u32);
    let d3 = (b[i + 2] as u32).wrapping_sub(b'0' as u32);
    let d4 = (b[i + 3] as u32).wrapping_sub(b'0' as u32);
    if d1 <= 9 && d2 <= 9 && d3 <= 9 && d4 <= 9 {
        Some(d1 * 1000 + d2 * 100 + d3 * 10 + d4)
    } else {
        None
    }
}

/// Days from Unix epoch (1970-01-01) for a given civil date.
/// Algorithm from Howard Hinnant's date library.
#[inline]
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if month < 1 || month > 12 || day < 1 {
        return None;
    }
    let is_leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => if is_leap { 29 } else { 28 },
        _ => return None,
    };
    if day > max_day {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 9 } else { month - 3 } as i64;
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * m + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}
