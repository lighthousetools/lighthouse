//! Finds free time between busy blocks. Performs no IO.

use chrono::{DateTime, Datelike, Duration, NaiveTime, TimeZone, Utc, Weekday};

/// Working hours to look in.
#[derive(Clone, Copy, Debug)]
pub struct WorkingHours {
    /// Start of the day.
    pub start: NaiveTime,
    /// End of the day.
    pub end: NaiveTime,
    /// Whether Saturday and Sunday count.
    pub weekends: bool,
}

/// Returns free slots of at least `duration` within working hours, earliest first.
/// Each slot starts on the hour or half hour and lasts exactly `duration`.
pub fn free_slots<Z: TimeZone>(
    busy: &[(DateTime<Utc>, DateTime<Utc>)],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    duration: Duration,
    hours: WorkingHours,
    zone: &Z,
    limit: usize,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut slots = Vec::new();
    let step = Duration::minutes(30);
    let mut day = from.with_timezone(zone).date_naive();
    let last = to.with_timezone(zone).date_naive();
    while day <= last && slots.len() < limit {
        let weekend = matches!(day.weekday(), Weekday::Sat | Weekday::Sun);
        if hours.weekends || !weekend {
            let local = |time: NaiveTime| {
                zone.from_local_datetime(&day.and_time(time))
                    .earliest()
                    .map(|time| time.with_timezone(&Utc))
            };
            if let (Some(open), Some(close)) = (local(hours.start), local(hours.end)) {
                let mut start = open;
                while start + duration <= close && slots.len() < limit {
                    let end = start + duration;
                    let free = start >= from
                        && end <= to
                        && !busy
                            .iter()
                            .any(|(busy_start, busy_end)| start < *busy_end && end > *busy_start);
                    if free {
                        slots.push((start, end));
                        start = end;
                    } else {
                        start += step;
                    }
                }
            }
        }
        day = day.succ_opt().unwrap_or(day);
        if day == from.with_timezone(zone).date_naive() {
            break;
        }
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, day, hour, minute, 0)
            .unwrap()
    }

    fn hours(weekends: bool) -> WorkingHours {
        WorkingHours {
            start: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            end: NaiveTime::from_hms_opt(12, 0, 0).unwrap(),
            weekends,
        }
    }

    #[test]
    fn slots_avoid_busy_time_and_weekends() {
        let busy = [(at(9, 9, 0), at(9, 10, 15))];
        let slots = free_slots(
            &busy,
            at(9, 0, 0),
            at(10, 23, 0),
            Duration::hours(1),
            hours(false),
            &Utc,
            10,
        );
        assert_eq!(slots[0], (at(9, 10, 30), at(9, 11, 30)));
        assert_eq!(slots[1], (at(10, 9, 0), at(10, 10, 0)));
        assert_eq!(slots.len(), 4);
        let saturday = free_slots(
            &[],
            at(7, 0, 0),
            at(7, 23, 0),
            Duration::hours(1),
            hours(false),
            &Utc,
            10,
        );
        assert!(saturday.is_empty());
        let weekend = free_slots(
            &[],
            at(7, 0, 0),
            at(7, 23, 0),
            Duration::hours(1),
            hours(true),
            &Utc,
            2,
        );
        assert_eq!(weekend.len(), 2);
        let late = free_slots(
            &[],
            at(9, 11, 10),
            at(9, 23, 0),
            Duration::hours(1),
            hours(false),
            &Utc,
            5,
        );
        assert!(late.is_empty(), "no slot starts before the range");
    }
}
