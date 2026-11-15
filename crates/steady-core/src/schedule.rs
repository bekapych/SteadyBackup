//! Расписание заданий.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Weekday};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Schedule {
    /// Только вручную.
    #[default]
    Manual,
    Once {
        at: NaiveDateTime,
    },
    Daily {
        times: Vec<NaiveTime>,
    },
    Weekly {
        /// "Mon", "Tue", ... "Sun"
        days: Vec<Weekday>,
        times: Vec<NaiveTime>,
    },
    Monthly {
        /// Числа месяца 1..31; 31 в коротком месяце = последний день.
        days: Vec<u32>,
        times: Vec<NaiveTime>,
    },
    Interval {
        minutes: u32,
        /// Окно времени, напр. с 08:00 до 20:00. Если `to` раньше `from` — окно через полночь (22:00–06:00).
        /// Если не задано — круглосуточно.
        #[serde(default)]
        from: Option<NaiveTime>,
        #[serde(default)]
        to: Option<NaiveTime>,
    },
}

impl Schedule {
    /// Ближайший запуск строго после `after`.
    pub fn next_after(&self, after: NaiveDateTime) -> Option<NaiveDateTime> {
        match self {
            Schedule::Manual => None,
            Schedule::Once { at } => (*at > after).then_some(*at),
            Schedule::Daily { times } => scan_days(after, times, |_| true),
            Schedule::Weekly { days, times } => scan_days(after, times, |d| days.contains(&d.weekday())),
            Schedule::Monthly { days, times } => scan_days(after, times, |d| {
                let last = last_day_of_month(d);
                days.iter().any(|&n| n == d.day() || (n > last && d.day() == last))
            }),
            Schedule::Interval { minutes, from, to } => {
                if *minutes == 0 {
                    return None;
                }
                let step = Duration::minutes(*minutes as i64);
                let from = from.unwrap_or(NaiveTime::MIN);
                // to < from — окно через полночь (22:00–06:00). to == from остаётся «до конца суток».
                let overnight = to.is_some_and(|t| t < from);
                for day_offset in -1..=1 {
                    let date = after.date() + Duration::days(day_offset);
                    let start = date.and_time(from);
                    let end = if overnight {
                        (date + Duration::days(1)).and_time(to.unwrap())
                    } else {
                        match to {
                            Some(t) if *t > from => date.and_time(*t),
                            _ => date.and_time(NaiveTime::MIN) + Duration::days(1) - Duration::seconds(1),
                        }
                    };
                    let mut t = start;
                    if after >= start {
                        let n = (after - start).num_seconds() / step.num_seconds() + 1;
                        t = start + step * n as i32;
                    }
                    if t > after && t <= end {
                        return Some(t);
                    }
                }
                None
            }
        }
    }
}

fn scan_days(after: NaiveDateTime, times: &[NaiveTime], day_ok: impl Fn(NaiveDate) -> bool) -> Option<NaiveDateTime> {
    if times.is_empty() {
        return None;
    }
    let mut sorted = times.to_vec();
    sorted.sort();
    // Достаточно 366 дней, чтобы найти любой день месяца/недели.
    for i in 0..=366 {
        let d = after.date() + Duration::days(i);
        if !day_ok(d) {
            continue;
        }
        if let Some(t) = sorted.iter().map(|t| d.and_time(*t)).find(|dt| *dt > after) {
            return Some(t);
        }
    }
    None
}

fn last_day_of_month(d: NaiveDate) -> u32 {
    let (y, m) = if d.month() == 12 { (d.year() + 1, 1) } else { (d.year(), d.month() + 1) };
    NaiveDate::from_ymd_opt(y, m, 1).unwrap().pred_opt().unwrap().day()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }
    fn t(s: &str) -> NaiveTime {
        NaiveTime::parse_from_str(s, "%H:%M").unwrap()
    }

    #[test]
    fn daily() {
        let s = Schedule::Daily { times: vec![t("22:00"), t("12:00")] };
        assert_eq!(s.next_after(dt("2026-11-09 10:00")), Some(dt("2026-11-09 12:00")));
        assert_eq!(s.next_after(dt("2026-11-09 12:00")), Some(dt("2026-11-09 22:00")));
        assert_eq!(s.next_after(dt("2026-11-09 23:00")), Some(dt("2026-11-10 12:00")));
    }

    #[test]
    fn weekly() {
        // 2026-11-09 — понедельник
        let s = Schedule::Weekly { days: vec![Weekday::Fri], times: vec![t("18:00")] };
        assert_eq!(s.next_after(dt("2026-11-09 10:00")), Some(dt("2026-11-13 18:00")));
        assert_eq!(s.next_after(dt("2026-11-13 18:00")), Some(dt("2026-11-20 18:00")));
    }

    #[test]
    fn monthly_last_day() {
        let s = Schedule::Monthly { days: vec![31], times: vec![t("23:00")] };
        assert_eq!(s.next_after(dt("2026-11-09 10:00")), Some(dt("2026-11-30 23:00")));
        assert_eq!(s.next_after(dt("2026-11-30 23:00")), Some(dt("2026-12-31 23:00")));
        assert_eq!(s.next_after(dt("2027-02-01 00:00")), Some(dt("2027-02-28 23:00")));
    }

    #[test]
    fn interval() {
        let s = Schedule::Interval { minutes: 90, from: Some(t("08:00")), to: Some(t("20:00")) };
        assert_eq!(s.next_after(dt("2026-11-09 07:00")), Some(dt("2026-11-09 08:00")));
        assert_eq!(s.next_after(dt("2026-11-09 08:00")), Some(dt("2026-11-09 09:30")));
        assert_eq!(s.next_after(dt("2026-11-09 19:40")), Some(dt("2026-11-09 20:00")));
        assert_eq!(s.next_after(dt("2026-11-09 20:00")), Some(dt("2026-11-10 08:00")));
        let s = Schedule::Interval { minutes: 15, from: None, to: None };
        assert_eq!(s.next_after(dt("2026-11-09 23:50")), Some(dt("2026-11-10 00:00")));
    }

    #[test]
    fn interval_across_midnight() {
        let s = Schedule::Interval { minutes: 60, from: Some(t("22:00")), to: Some(t("06:00")) };
        assert_eq!(s.next_after(dt("2026-11-09 21:00")), Some(dt("2026-11-09 22:00")));
        assert_eq!(s.next_after(dt("2026-11-09 23:30")), Some(dt("2026-11-10 00:00")));
        assert_eq!(s.next_after(dt("2026-11-10 03:10")), Some(dt("2026-11-10 04:00")));
        assert_eq!(s.next_after(dt("2026-11-10 05:30")), Some(dt("2026-11-10 06:00")));
        assert_eq!(s.next_after(dt("2026-11-10 06:00")), Some(dt("2026-11-10 22:00")));
        assert_eq!(s.next_after(dt("2026-11-10 12:00")), Some(dt("2026-11-10 22:00")));
    }

    #[test]
    fn json() {
        let s: Schedule = serde_json::from_str(r#"{"type":"weekly","days":["Mon","Fri"],"times":["21:30:00"]}"#).unwrap();
        assert!(matches!(s, Schedule::Weekly { .. }));
    }
}
