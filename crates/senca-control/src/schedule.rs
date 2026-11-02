//! 予約の検査（重なり・同時数）と、繰り返しの展開。
//!
//! 繰り返しは壁時計（IANA タイムゾーン）で解釈する。タイムゾーンの規則は TZif（RFC 8536）の
//! バイト列を [`Tz::parse_tzif`] に渡して使う（ファイルを読むのは呼び出し側）。
//! 夏時間の決まり: 存在しない時刻（春の飛び）は後ろへずらし、重なる時刻（秋の戻り）は最初の方を使う。

use crate::model::{Recurrence, Schedule, ScheduleState};

/// 1 つの stream で同時に動ける録画予約の数。
pub const MAX_CONCURRENT_PER_STREAM: usize = 4;
/// 予約 1 件の長さの上限（24 時間）。
pub const MAX_WINDOW_MS: i64 = 24 * 3600 * 1000;
/// 繰り返しの展開数の上限。
pub const MAX_OCCURRENCES: u32 = 1000;

/// 予約の時間（pre/post-roll を含む）が重なる、まだ終わっていない予約の最大の同時数。
pub fn max_concurrent(existing: &[&Schedule], start: i64, end: i64) -> usize {
    // 区間の端点を並べて走査する
    let mut ev: Vec<(i64, i32)> = Vec::new();
    for s in existing {
        if matches!(s.state, ScheduleState::Completed | ScheduleState::Canceled | ScheduleState::Missed | ScheduleState::Failed) {
            continue;
        }
        let a = s.start_at_ms - s.pre_roll_ms as i64;
        let b = s.end_at_ms + s.post_roll_ms as i64;
        if a < end && start < b {
            ev.push((a.max(start), 1));
            ev.push((b.min(end), -1));
        }
    }
    ev.sort_by(|x, y| x.0.cmp(&y.0).then(x.1.cmp(&y.1)));
    let (mut cur, mut best) = (0i32, 0i32);
    for (_, d) in ev {
        cur += d;
        best = best.max(cur);
    }
    best as usize
}

/// TZif の規則（遷移の一覧と、それ以降の POSIX TZ 文字列は使わず、遷移表の最後の値を延長する）。
#[derive(Debug, Clone)]
pub struct Tz {
    /// (遷移の UTC 秒, その後のオフセット秒)
    transitions: Vec<(i64, i32)>,
    initial_offset: i32,
    /// 遷移表の後ろの規則（POSIX TZ）。
    footer: Option<PosixTz>,
}

#[derive(Debug, Clone)]
struct PosixTz {
    std_offset: i32,
    dst: Option<(i32, Rule, Rule)>,
}

#[derive(Debug, Clone, Copy)]
struct Rule {
    // Mm.w.d/time のみ対応
    month: u32,
    week: u32,
    wday: u32,
    secs: i32,
}

impl Tz {
    pub const UTC: fn() -> Tz = || Tz { transitions: Vec::new(), initial_offset: 0, footer: None };

    /// TZif 版 2 以降（64 bit の表）を読む。壊れていれば `None`。
    pub fn parse_tzif(b: &[u8]) -> Option<Tz> {
        fn be32(b: &[u8], i: usize) -> Option<i32> {
            Some(i32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?))
        }
        fn be64(b: &[u8], i: usize) -> Option<i64> {
            Some(i64::from_be_bytes(b.get(i..i + 8)?.try_into().ok()?))
        }
        if b.get(0..4)? != b"TZif" {
            return None;
        }
        let hdr = |off: usize| -> Option<[usize; 6]> {
            let mut c = [0usize; 6];
            for (k, slot) in c.iter_mut().enumerate() {
                let v = be32(b, off + 20 + k * 4)?;
                if !(0..=100_000).contains(&v) {
                    return None;
                }
                *slot = v as usize;
            }
            Some(c)
        };
        // c = [isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt]
        let c1 = hdr(0)?;
        let v1_len = 44 + c1[3] * 5 + c1[4] * 6 + c1[5] + c1[2] * 8 + c1[1] + c1[0];
        let version = *b.get(4)?;
        if version < b'2' {
            return None;
        }
        let off = v1_len;
        if b.get(off..off + 4)? != b"TZif" {
            return None;
        }
        let c = hdr(off)?;
        let mut p = off + 44;
        let mut times = Vec::with_capacity(c[3]);
        for i in 0..c[3] {
            times.push(be64(b, p + i * 8)?);
        }
        p += c[3] * 8;
        let idx: Vec<u8> = b.get(p..p + c[3])?.to_vec();
        p += c[3];
        let mut types = Vec::with_capacity(c[4]);
        for i in 0..c[4] {
            types.push(be32(b, p + i * 6)?);
        }
        p += c[4] * 6 + c[5] + c[2] * 12 + c[1] + c[0];
        let mut transitions = Vec::new();
        for (t, i) in times.into_iter().zip(idx) {
            transitions.push((t, *types.get(i as usize)?));
        }
        let initial_offset = *types.first()?;
        let footer = b
            .get(p..)
            .and_then(|f| std::str::from_utf8(f).ok())
            .and_then(|f| f.trim_matches('\n').lines().next().map(str::to_string))
            .and_then(|s| parse_posix(&s));
        Some(Tz { transitions, initial_offset, footer })
    }

    /// UTC 秒 → オフセット秒。
    pub fn offset_at(&self, utc: i64) -> i32 {
        match self.transitions.binary_search_by(|(t, _)| t.cmp(&utc)) {
            Ok(i) => self.transitions[i].1,
            Err(0) => self.initial_offset,
            Err(i) => {
                if i == self.transitions.len()
                    && let Some(f) = &self.footer
                {
                    return f.offset_at(utc);
                }
                self.transitions[i - 1].1
            }
        }
    }

    /// 壁時計（ローカル秒、エポックからの日数×86400＋時刻）→ UTC 秒。
    /// 存在しない時刻は後ろへずらし、重なる時刻は早い方。
    pub fn local_to_utc(&self, local: i64) -> i64 {
        // 候補のオフセットを両方試す
        let mut cands: Vec<i64> = Vec::new();
        for probe in [local - 86_400, local, local + 86_400] {
            let off = self.offset_at(probe) as i64;
            let u = local - off;
            if self.offset_at(u) as i64 == off && !cands.contains(&u) {
                cands.push(u);
            }
        }
        if let Some(m) = cands.into_iter().min() {
            return m;
        }
        // 飛ばされた時刻: 前のオフセットで換算すると遷移の後ろへ落ちる
        let before = self.offset_at(local - 86_400) as i64;
        local - before
    }
}

impl PosixTz {
    fn offset_at(&self, utc: i64) -> i32 {
        let Some((dst_off, start, end)) = &self.dst else {
            return self.std_offset;
        };
        let year = civil_year(utc + self.std_offset as i64);
        let s = start.utc_for_year(year, self.std_offset);
        let e = end.utc_for_year(year, *dst_off);
        let in_dst = if s < e { utc >= s && utc < e } else { utc >= s || utc < e };
        if in_dst { *dst_off } else { self.std_offset }
    }
}

impl Rule {
    fn utc_for_year(&self, year: i64, offset_before: i32) -> i64 {
        // 月の 1 日の曜日から w 番目の d 曜日（5 は最後）
        let first = days_from_civil(year, self.month as i64, 1);
        let wd_first = (first + 4).rem_euclid(7) as u32; // 1970-01-01 は木曜(4)
        let mut day = 1 + ((self.wday + 7 - wd_first) % 7) as i64 + (self.week as i64 - 1) * 7;
        let dim = days_in_month(year, self.month as i64);
        while day > dim {
            day -= 7;
        }
        let local = (first + day - 1) * 86_400 + self.secs as i64;
        local - offset_before as i64
    }
}

fn parse_posix(s: &str) -> Option<PosixTz> {
    let b = s.as_bytes();
    let mut i = 0;
    let name = |i: &mut usize| -> Option<()> {
        if b.get(*i) == Some(&b'<') {
            while *b.get(*i)? != b'>' {
                *i += 1;
            }
            *i += 1;
        } else {
            let st = *i;
            while b.get(*i).is_some_and(|c| c.is_ascii_alphabetic()) {
                *i += 1;
            }
            if *i - st < 3 {
                return None;
            }
        }
        Some(())
    };
    let offset = |i: &mut usize| -> Option<i32> {
        let mut sign = 1;
        if let Some(c @ (b'+' | b'-')) = b.get(*i) {
            sign = if *c == b'-' { -1 } else { 1 };
            *i += 1;
        }
        let mut parts = [0i32; 3];
        for (k, part) in parts.iter_mut().enumerate() {
            if k > 0 {
                if b.get(*i) != Some(&b':') {
                    break;
                }
                *i += 1;
            }
            let st = *i;
            while b.get(*i).is_some_and(u8::is_ascii_digit) {
                *i += 1;
            }
            *part = s.get(st..*i)?.parse().ok()?;
        }
        Some(sign * (parts[0] * 3600 + parts[1] * 60 + parts[2]))
    };
    name(&mut i)?;
    // POSIX の符号は逆（UTC からの西向き）
    let std_offset = -offset(&mut i)?;
    if i >= b.len() {
        return Some(PosixTz { std_offset, dst: None });
    }
    name(&mut i)?;
    let mut dst_offset = std_offset + 3600;
    if b.get(i) != Some(&b',') {
        dst_offset = -offset(&mut i)?;
    }
    let rule = |i: &mut usize| -> Option<Rule> {
        if b.get(*i) != Some(&b',') || b.get(*i + 1) != Some(&b'M') {
            return None;
        }
        *i += 2;
        let mut nums = [0u32; 3];
        for (k, n) in nums.iter_mut().enumerate() {
            if k > 0 {
                if b.get(*i) != Some(&b'.') {
                    return None;
                }
                *i += 1;
            }
            let st = *i;
            while b.get(*i).is_some_and(u8::is_ascii_digit) {
                *i += 1;
            }
            *n = s.get(st..*i)?.parse().ok()?;
        }
        let mut secs = 7200;
        if b.get(*i) == Some(&b'/') {
            *i += 1;
            secs = offset(i)?;
        }
        if !(1..=12).contains(&nums[0]) || !(1..=5).contains(&nums[1]) || nums[2] > 6 {
            return None;
        }
        Some(Rule { month: nums[0], week: nums[1], wday: nums[2], secs })
    };
    let start = rule(&mut i)?;
    let end = rule(&mut i)?;
    Some(PosixTz { std_offset, dst: Some((dst_offset, start, end)) })
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_year(secs: i64) -> i64 {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let y = yoe + era * 400;
    if mp >= 10 { y + 1 } else { y }
}

/// 繰り返しを展開する。最初の回は `start_ms`〜`start_ms + len`。2 回目以降は、最初の回の
/// 壁時計の時刻を保ったまま日付を進める。`from_ms` 以降に終わる回だけを最大 `limit` 件返す。
pub fn expand(start_ms: i64, len_ms: i64, rec: &Recurrence, tz: &Tz, from_ms: i64, limit: usize) -> Vec<(u32, i64, i64)> {
    let mut out = Vec::new();
    let start_s = start_ms.div_euclid(1000);
    let local0 = start_s + tz.offset_at(start_s) as i64;
    let day0 = local0.div_euclid(86_400);
    let tod = local0.rem_euclid(86_400);
    let sub_ms = start_ms.rem_euclid(1000);
    let max = rec.count.unwrap_or(MAX_OCCURRENCES).min(MAX_OCCURRENCES);
    let mut n = 0u32;
    // 最大 400 週ぶん見る
    for day in day0..day0 + 7 * 400 {
        if n >= max || out.len() >= limit {
            break;
        }
        let ok = match rec.freq.as_str() {
            "daily" => true,
            "weekly" => {
                let wd = (day + 4).rem_euclid(7) as u8;
                if rec.weekdays.is_empty() { day == day0 || (day - day0) % 7 == 0 } else { rec.weekdays.contains(&wd) }
            }
            _ => false,
        };
        if ok && day >= day0 {
            let s = tz.local_to_utc(day * 86_400 + tod) * 1000 + sub_ms;
            if let Some(u) = rec.until_ms
                && s >= u
            {
                break;
            }
            let e = s + len_ms;
            if e > from_ms {
                out.push((n, s, e));
            }
            n += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sched(start: i64, end: i64) -> Schedule {
        Schedule {
            id: "x".into(),
            tenant: "t".into(),
            stream: "s".into(),
            generation: 1,
            name: None,
            action: crate::model::ScheduleAction::Record,
            start_at_ms: start,
            end_at_ms: end,
            pre_roll_ms: 0,
            post_roll_ms: 0,
            grace_ms: 0,
            snapshot_interval_secs: None,
            recurrence: None,
            retention_days: None,
            state: ScheduleState::Pending,
            runs: vec![],
            created_at_ms: 0,
        }
    }

    #[test]
    fn concurrency() {
        let a = sched(0, 100);
        let b = sched(50, 150);
        let c = sched(100, 200);
        let v = vec![&a, &b, &c];
        assert_eq!(max_concurrent(&v, 0, 200), 2);
        assert_eq!(max_concurrent(&v, 60, 90), 2);
        assert_eq!(max_concurrent(&v, 160, 170), 1);
        assert_eq!(max_concurrent(&v, 300, 400), 0);
    }

    #[test]
    fn posix_rules() {
        // 米国東部: EST5EDT,M3.2.0,M11.1.0
        let p = parse_posix("EST5EDT,M3.2.0,M11.1.0").unwrap();
        // 2026-07-01T12:00Z は夏時間 (-4h)
        assert_eq!(p.offset_at(1_782_907_200), -4 * 3600);
        // 2026-01-15T12:00Z は標準時
        assert_eq!(p.offset_at(1_768_478_400), -5 * 3600);
        let j = parse_posix("JST-9").unwrap();
        assert_eq!(j.offset_at(0), 9 * 3600);
    }

    #[test]
    fn real_zoneinfo_if_present() {
        let Ok(b) = std::fs::read("/usr/share/zoneinfo/America/New_York") else { return };
        let tz = Tz::parse_tzif(&b).unwrap();
        // 2026-03-08 02:30 は存在しない → 03:30 EDT = 07:30Z
        let day = days_from_civil(2026, 3, 8);
        let u = tz.local_to_utc(day * 86_400 + 2 * 3600 + 1800);
        assert_eq!(u, day * 86_400 + 7 * 3600 + 1800);
        // 2026-11-01 01:30 は 2 回ある → 早い方（EDT）= 05:30Z
        let day = days_from_civil(2026, 11, 1);
        assert_eq!(tz.local_to_utc(day * 86_400 + 3600 + 1800), day * 86_400 + 5 * 3600 + 1800);
        // 毎日 10:00 の繰り返しが夏時間をまたいでも壁時計 10:00
        let rec = Recurrence { freq: "daily".into(), weekdays: vec![], timezone: "America/New_York".into(), until_ms: None, count: Some(3) };
        let start = (days_from_civil(2026, 3, 7) * 86_400 + 15 * 3600) * 1000; // 10:00 EST
        let occ = expand(start, 3_600_000, &rec, &tz, 0, 10);
        assert_eq!(occ.len(), 3);
        assert_eq!(occ[1].1, (days_from_civil(2026, 3, 8) * 86_400 + 14 * 3600) * 1000); // 10:00 EDT
    }
}
