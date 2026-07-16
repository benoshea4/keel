// cron.rs — v2.1 cron schedules: a 6-field parser (SECONDS resolution) and the
// next-fire computation for BOTH schedule kinds (cron and interval), so the
// scheduler loop has exactly one place that decides "when next".
//
// Fields: sec min hour day-of-month month day-of-week — all UTC, vixie-cron
// semantics: each field is a comma list of `*`, `N`, `A-B`, optionally `/step`;
// day-of-week 0-7 with both 0 and 7 = Sunday; when BOTH dom and dow are
// restricted (not `*`), a day matches if EITHER does. Hand-rolled on purpose
// (like /metrics): a chrono dependency for six bitmasks and forty lines of
// civil-date math is not the lean-engine trade.

use anyhow::{bail, Result};

#[derive(Debug, Clone)]
pub struct Cron {
    sec: u64,  // bits 0-59
    min: u64,  // bits 0-59
    hour: u32, // bits 0-23
    dom: u32,  // bits 1-31
    mon: u16,  // bits 1-12
    dow: u8,   // bits 0-6, 0 = Sunday
    dom_star: bool,
    dow_star: bool,
}

/// One cron field into a bitmask. `names` bounds are inclusive.
fn parse_field(field: &str, lo: u32, hi: u32, what: &str) -> Result<(u64, bool)> {
    if field.is_empty() {
        bail!("empty {what} field");
    }
    let mut mask: u64 = 0;
    let mut is_star = true;
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad step '{s}' in {what}"))?;
                if step == 0 {
                    bail!("step 0 in {what}");
                }
                (r, step)
            }
            None => (part, 1),
        };
        let (a, b) = if range == "*" {
            (lo, hi)
        } else {
            is_star = false;
            match range.split_once('-') {
                Some((a, b)) => {
                    let a: u32 = a.parse().map_err(|_| anyhow::anyhow!("bad number '{a}' in {what}"))?;
                    let b: u32 = b.parse().map_err(|_| anyhow::anyhow!("bad number '{b}' in {what}"))?;
                    (a, b)
                }
                None => {
                    let n: u32 = range
                        .parse()
                        .map_err(|_| anyhow::anyhow!("bad number '{range}' in {what}"))?;
                    (n, n)
                }
            }
        };
        // A bare `*` stays "unrestricted"; `*/step` restricts.
        if range == "*" && step != 1 {
            is_star = false;
        }
        if a < lo || b > hi || a > b {
            bail!("{what} value out of range: '{part}' (allowed {lo}-{hi})");
        }
        let mut v = a;
        while v <= b {
            mask |= 1 << v;
            v += step;
        }
    }
    Ok((mask, is_star))
}

pub fn parse(expr: &str) -> Result<Cron> {
    let f: Vec<&str> = expr.split_whitespace().collect();
    if f.len() != 6 {
        bail!(
            "cron needs 6 fields (sec min hour day-of-month month day-of-week), got {} in '{expr}'",
            f.len()
        );
    }
    let (sec, _) = parse_field(f[0], 0, 59, "second")?;
    let (min, _) = parse_field(f[1], 0, 59, "minute")?;
    let (hour, _) = parse_field(f[2], 0, 23, "hour")?;
    let (dom, dom_star) = parse_field(f[3], 1, 31, "day-of-month")?;
    let (mon, _) = parse_field(f[4], 1, 12, "month")?;
    // dow: accept 0-7, fold 7 (also Sunday) onto bit 0.
    let (dow_raw, dow_star) = parse_field(f[5], 0, 7, "day-of-week")?;
    let dow = ((dow_raw | (dow_raw >> 7)) & 0x7f) as u8;
    Ok(Cron {
        sec,
        min,
        hour: hour as u32,
        dom: dom as u32,
        mon: mon as u16,
        dow,
        dom_star,
        dow_star,
    })
}

/// Proleptic-Gregorian date from days since 1970-01-01 (Howard Hinnant's
/// civil_from_days — the standard branchless algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl Cron {
    fn day_matches(&self, dom: u32, dow: u32) -> bool {
        let dom_ok = self.dom & (1 << dom) != 0;
        let dow_ok = self.dow & (1 << dow) != 0;
        match (self.dom_star, self.dow_star) {
            (true, true) => true,
            (false, true) => dom_ok,
            (true, false) => dow_ok,
            // Both restricted: vixie-cron ORs them.
            (false, false) => dom_ok || dow_ok,
        }
    }

    /// Next matching UTC instant STRICTLY AFTER `after_ms`, as unix millis.
    /// None if nothing matches within ~4 years (an impossible date like Feb 30).
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        let mut t = after_ms.div_euclid(1000) + 1; // candidate, epoch seconds
        for _ in 0..(4 * 366) {
            // one iteration per candidate DAY, bounded
            let days = t.div_euclid(86_400);
            let (_, mon, dom) = civil_from_days(days);
            let dow = ((days + 4).rem_euclid(7)) as u32; // 1970-01-01 = Thursday, Sunday = 0
            if self.mon & (1 << mon) == 0 || !self.day_matches(dom, dow) {
                t = (days + 1) * 86_400;
                continue;
            }
            let day0 = days * 86_400;
            let rem = (t - day0) as u32; // second-of-day we may start from
            let (h0, m0, s0) = (rem / 3600, (rem % 3600) / 60, rem % 60);
            for h in h0..24 {
                if self.hour & (1 << h) == 0 {
                    continue;
                }
                let m_start = if h == h0 { m0 } else { 0 };
                for m in m_start..60 {
                    if self.min & (1 << m) == 0 {
                        continue;
                    }
                    let s_start = if h == h0 && m == m0 { s0 } else { 0 };
                    for s in s_start..60 {
                        if self.sec & (1 << s) != 0 {
                            return Some((day0 + (h * 3600 + m * 60 + s) as i64) * 1000);
                        }
                    }
                }
            }
            t = (days + 1) * 86_400; // nothing left today
        }
        None
    }
}

/// The scheduler's single "when next" decision, for both kinds. Interval
/// schedules keep their phase (whole intervals from the ORIGINAL next_run_at,
/// so downtime collapses to one firing without drift); cron schedules compute
/// from now, which collapses missed windows by construction. None = this
/// schedule can never fire again (undefined cron date) — caller disables it.
pub fn next_run(cron_expr: Option<&str>, interval_ms: i64, prev_next: i64, now: i64) -> Option<i64> {
    match cron_expr {
        Some(expr) => parse(expr).ok()?.next_after(now),
        None => {
            let missed = (now - prev_next).max(0) / interval_ms.max(1);
            Some(prev_next + (missed + 1) * interval_ms.max(1))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-07-15 12:00:00 UTC (a Wednesday) in unix millis.
    const WED_NOON: i64 = 1_784_116_800_000;

    #[test]
    fn every_two_seconds() {
        let c = parse("*/2 * * * * *").unwrap();
        let n1 = c.next_after(WED_NOON).unwrap();
        let n2 = c.next_after(n1).unwrap();
        assert_eq!(n1, WED_NOON + 2000);
        assert_eq!(n2 - n1, 2000);
    }

    #[test]
    fn strictly_after_even_on_exact_match() {
        let c = parse("0 0 12 * * *").unwrap(); // noon exactly
        assert_eq!(c.next_after(WED_NOON).unwrap(), WED_NOON + 86_400_000);
    }

    #[test]
    fn top_of_hour() {
        let c = parse("0 0 * * * *").unwrap();
        assert_eq!(c.next_after(WED_NOON + 1000).unwrap(), WED_NOON + 3_600_000);
    }

    #[test]
    fn weekday_mornings() {
        // 09:30:00 Mon-Fri. From Wednesday noon → Thursday 09:30.
        let c = parse("0 30 9 * * 1-5").unwrap();
        let n = c.next_after(WED_NOON).unwrap();
        let expected = WED_NOON - 12 * 3_600_000 + 86_400_000 + (9 * 3600 + 30 * 60) * 1000;
        assert_eq!(n, expected);
        // From Friday 10:00 → Monday 09:30 (skips the weekend).
        let fri_10 = WED_NOON + 2 * 86_400_000 - 2 * 3_600_000;
        let n = c.next_after(fri_10).unwrap();
        assert_eq!(n, fri_10 - 1800 * 1000 + 3 * 86_400_000);
    }

    #[test]
    fn sunday_as_seven() {
        assert_eq!(parse("0 0 0 * * 7").unwrap().dow, parse("0 0 0 * * 0").unwrap().dow);
    }

    #[test]
    fn month_rollover_and_leap_day() {
        // Feb 29 exists in 2028; from mid-2026 the next one is ~2y out but
        // within the 4-year search window.
        let c = parse("0 0 0 29 2 *").unwrap();
        let n = c.next_after(WED_NOON).unwrap();
        let days = n / 86_400_000;
        assert_eq!(civil_from_days(days), (2028, 2, 29));
    }

    #[test]
    fn impossible_date_is_none() {
        assert!(parse("0 0 0 31 2 *").unwrap().next_after(WED_NOON).is_none());
    }

    #[test]
    fn dom_or_dow_when_both_restricted() {
        // Vixie semantics: day 15 OR any Sunday.
        let c = parse("0 0 0 15 * 0").unwrap();
        let mut t = WED_NOON;
        let mut hits = Vec::new();
        for _ in 0..6 {
            t = c.next_after(t).unwrap();
            let days = t / 86_400_000;
            let (_, _, d) = civil_from_days(days);
            let dow = (days + 4) % 7;
            hits.push((d, dow));
        }
        assert!(hits.iter().all(|&(d, w)| d == 15 || w == 0), "{hits:?}");
        assert!(hits.iter().any(|&(d, _)| d == 15));
        assert!(hits.iter().any(|&(_, w)| w == 0));
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "* * * * *",          // 5 fields
            "60 * * * * *",       // sec out of range
            "* * 24 * * *",       // hour out of range
            "* * * 0 * *",        // dom below range
            "* * * * 13 *",       // month out of range
            "* * * * * 8",        // dow out of range
            "*/0 * * * * *",      // zero step
            "a * * * * *",        // not a number
            "5-2 * * * * *",      // inverted range
            "",                   // empty
        ] {
            assert!(parse(bad).is_err(), "'{bad}' should not parse");
        }
    }

    #[test]
    fn interval_next_keeps_phase_and_collapses_misses() {
        // Schedule was due at t=1000, interval 500, now t=2600 (3 windows
        // missed) → one firing, next at 3000 (still on the original phase).
        assert_eq!(next_run(None, 500, 1000, 2600), Some(3000));
        // On-time firing: due at 1000, now 1000 → next 1500.
        assert_eq!(next_run(None, 500, 1000, 1000), Some(1500));
    }

    #[test]
    fn cron_next_ignores_prev() {
        let n = next_run(Some("*/2 * * * * *"), 0, 0, WED_NOON);
        assert_eq!(n, Some(WED_NOON + 2000));
    }
}
