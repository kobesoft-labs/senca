//! UTC の RFC 3339 表記（`2026-10-25T00:00:00Z`、ミリ秒は任意）。外部の日付ライブラリを使わない。

/// UNIX 時刻（ミリ秒）を `YYYY-MM-DDTHH:MM:SS.sssZ` にする。ミリ秒が 0 なら秒まで。
pub fn format_millis(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let milli = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (sod / 3600, (sod / 60) % 60, sod % 60);
    if milli == 0 {
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
    } else {
        format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{milli:03}Z")
    }
}

/// RFC 3339 を UNIX 時刻（ミリ秒）にする。`Z` または `±HH:MM` を受け付ける。小数秒はミリ秒に切り捨て。
pub fn parse_millis(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        if !t.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        t.parse().ok()
    };
    if b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b't') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut i = 19;
    let mut milli = 0i64;
    if b[i] == b'.' {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
        let frac = &s[start..i];
        let mut f = frac.chars().take(3).collect::<String>();
        while f.len() < 3 {
            f.push('0');
        }
        milli = f.parse().ok()?;
    }
    let offset_min = match b.get(i)? {
        b'Z' | b'z' if i + 1 == b.len() => 0,
        c @ (b'+' | b'-') if i + 6 == b.len() && b[i + 3] == b':' => {
            let oh = num(i + 1..i + 3)?;
            let om = num(i + 4..i + 6)?;
            if oh > 23 || om > 59 {
                return None;
            }
            let o = oh * 60 + om;
            if *c == b'+' { o } else { -o }
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    let sec = sec.min(59); // うるう秒は 59 秒として扱う
    let t = days * 86_400 + h * 3600 + mi * 60 + sec - offset_min * 60;
    Some(t * 1000 + milli)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

// Howard Hinnant の days_from_civil / civil_from_days。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for ms in [0i64, 1_761_350_400_000, 1_761_350_400_123, 951_782_400_000, -1] {
            assert_eq!(parse_millis(&format_millis(ms)), Some(ms), "{}", format_millis(ms));
        }
        assert_eq!(format_millis(1_761_350_400_000), "2025-10-25T00:00:00Z");
    }

    #[test]
    fn offsets_and_rejects() {
        assert_eq!(parse_millis("2026-10-25T09:00:00+09:00"), parse_millis("2026-10-25T00:00:00Z"));
        assert_eq!(parse_millis("2026-02-29T00:00:00Z"), None);
        assert_eq!(parse_millis("2026-10-25 00:00:00Z"), None);
        assert_eq!(parse_millis("2026-10-25T00:00:00"), None);
        assert_eq!(parse_millis("2026-10-25T00:00:00.5Z"), Some(parse_millis("2026-10-25T00:00:00Z").unwrap() + 500));
    }
}
