//! The words a person reads: notifications, calendar names, descriptions.
//! Chosen by `calendar.language`.

use chrono::{Datelike, NaiveDate};
use serde::Deserialize;

use crate::reminder::ReminderAnchor;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    En,
    Ru,
}

impl Language {
    /// What a reminder is about, and whether that moment has passed.
    pub fn anchor(self, anchor: ReminderAnchor, past: bool) -> &'static str {
        match (self, anchor, past) {
            (Language::En, ReminderAnchor::Deadline, false) => "Deadline",
            (Language::En, ReminderAnchor::Deadline, true) => "Deadline was",
            (Language::En, ReminderAnchor::Scheduled, false) => "Planned",
            (Language::En, ReminderAnchor::Scheduled, true) => "Was planned",
            (Language::En, ReminderAnchor::Start, false) => "Starts",
            (Language::En, ReminderAnchor::Start, true) => "Started",
            (Language::Ru, ReminderAnchor::Deadline, false) => "Дедлайн",
            (Language::Ru, ReminderAnchor::Deadline, true) => "Дедлайн был",
            (Language::Ru, ReminderAnchor::Scheduled, false) => "По плану",
            (Language::Ru, ReminderAnchor::Scheduled, true) => "По плану было",
            (Language::Ru, ReminderAnchor::Start, false) => "Начало",
            (Language::Ru, ReminderAnchor::Start, true) => "Началось",
        }
    }

    /// A day, named against today.
    pub fn day(self, day: NaiveDate, today: NaiveDate) -> String {
        const EN: [&str; 12] = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        const RU: [&str; 12] = [
            "января",
            "февраля",
            "марта",
            "апреля",
            "мая",
            "июня",
            "июля",
            "августа",
            "сентября",
            "октября",
            "ноября",
            "декабря",
        ];
        let month = day.month0() as usize;
        match (self, (day - today).num_days()) {
            (Language::En, 0) => "today".to_string(),
            (Language::En, 1) => "tomorrow".to_string(),
            (Language::En, -1) => "yesterday".to_string(),
            (Language::En, _) => format!("on {} {}", day.day(), EN[month]),
            (Language::Ru, 0) => "сегодня".to_string(),
            (Language::Ru, 1) => "завтра".to_string(),
            (Language::Ru, -1) => "вчера".to_string(),
            (Language::Ru, _) => format!("{} {}", day.day(), RU[month]),
        }
    }

    /// A clock time after a day: "at 18:00".
    pub fn at(self, time: impl std::fmt::Display) -> String {
        match self {
            Language::En => format!("at {time}"),
            Language::Ru => format!("в {time}"),
        }
    }

    /// How long until a moment, in whole minutes or hours.
    ///
    /// Both units round to the nearest, from the raw seconds.
    ///
    /// Truncating reads as an off-by-one to anyone who chose the lead time: the
    /// scheduler wakes on a 30-second tick, so a reminder set an hour ahead is
    /// sent at 59m40s and "через 59 минут" is what the phone shows. Deriving the
    /// hours from the minutes instead would round twice, turning 1h29m40s into
    /// "через 2 часа" by way of 90 minutes.
    pub fn in_time(self, delta: chrono::Duration) -> String {
        let seconds = delta.num_seconds();
        let minutes = (seconds + 30) / 60;
        if minutes < 1 {
            return match self {
                Language::En => "in less than a minute".to_string(),
                Language::Ru => "меньше чем через минуту".to_string(),
            };
        }
        if minutes < 60 {
            return match (self, plural(minutes)) {
                (Language::En, _) if minutes == 1 => "in a minute".to_string(),
                (Language::En, _) => format!("in {minutes} minutes"),
                (Language::Ru, Plural::One) => "через минуту".to_string(),
                (Language::Ru, Plural::Few) => format!("через {minutes} минуты"),
                (Language::Ru, Plural::Many) => format!("через {minutes} минут"),
            };
        }
        let hours = (seconds + 1800) / 3600;
        match (self, plural(hours)) {
            (Language::En, _) if hours == 1 => "in an hour".to_string(),
            (Language::En, _) => format!("in {hours} hours"),
            (Language::Ru, Plural::One) => "через час".to_string(),
            (Language::Ru, Plural::Few) => format!("через {hours} часа"),
            (Language::Ru, Plural::Many) => format!("через {hours} часов"),
        }
    }

    /// The deadline a task's description carries when the calendar cannot
    /// show it.
    pub fn deadline_line(self, date: NaiveDate, time: Option<impl std::fmt::Display>) -> String {
        let date = match self {
            Language::En => date.format("%Y-%m-%d").to_string(),
            Language::Ru => date.format("%d.%m.%Y").to_string(),
        };
        let when = match time {
            Some(time) => format!("{date} {time}"),
            None => date,
        };
        match self {
            Language::En => format!("Deadline: {when}"),
            Language::Ru => format!("Дедлайн: {when}"),
        }
    }

    /// The name of the entry that stands for an event's deadline.
    pub fn deadline_entry(self, name: &str) -> String {
        match self {
            Language::En => format!("{name} (deadline)"),
            Language::Ru => format!("{name} (дедлайн)"),
        }
    }

    /// The calendar of tasks assigned to nobody.
    pub fn unassigned(self) -> &'static str {
        match self {
            Language::En => "Unassigned",
            Language::Ru => "Без исполнителя",
        }
    }

    /// The events calendar, unless the config names it.
    pub fn events(self) -> &'static str {
        match self {
            Language::En => "Events",
            Language::Ru => "События",
        }
    }

    /// The body of the notification sent by `/push/test`.
    pub fn test_notification(self) -> &'static str {
        match self {
            Language::En => "Test notification from anytype-caldav",
            Language::Ru => "Тестовое уведомление от anytype-caldav",
        }
    }
}

enum Plural {
    One,
    Few,
    Many,
}

/// Russian counts agree in three forms, chosen by the last digits.
fn plural(count: i64) -> Plural {
    match (count % 10, count % 100) {
        (1, tens) if tens != 11 => Plural::One,
        (2..=4, tens) if !(12..=14).contains(&tens) => Plural::Few,
        _ => Plural::Many,
    }
}
