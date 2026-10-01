use jiff::Span;
use jiff::civil::DateTime;

/// A mail-client date: the time today, "Yesterday", the weekday earlier
/// this week, and the month and day before that.
pub fn short(now: DateTime, at: DateTime) -> String {
    let today = now.date();
    let day = at.date();
    if day == today {
        return at.strftime("%-I:%M %p").to_string();
    }
    if today.yesterday().is_ok_and(|y| y == day) {
        return "Yesterday".to_owned();
    }
    let since_monday = i64::from(today.weekday().to_monday_zero_offset());
    let monday = today
        .checked_sub(Span::new().days(since_monday))
        .unwrap_or(today);
    if day >= monday && day < today {
        return at.strftime("%a").to_string();
    }
    at.strftime("%b %-d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    #[test]
    fn formats_relative_to_now() {
        // Thursday.
        let now = date(2026, 10, 1).at(12, 0, 0, 0);
        assert_eq!(short(now, date(2026, 10, 1).at(9, 12, 0, 0)), "9:12 AM");
        assert_eq!(short(now, date(2026, 10, 1).at(14, 5, 0, 0)), "2:05 PM");
        assert_eq!(short(now, date(2026, 9, 30).at(9, 0, 0, 0)), "Yesterday");
        assert_eq!(short(now, date(2026, 9, 28).at(9, 0, 0, 0)), "Mon");
        assert_eq!(short(now, date(2026, 9, 27).at(9, 0, 0, 0)), "Sep 27");
        assert_eq!(short(now, date(2026, 10, 2).at(9, 0, 0, 0)), "Oct 2");
    }
}
