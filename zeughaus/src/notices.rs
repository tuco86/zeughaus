//! The notices the notice popout lists: what ended without anyone watching,
//! an agent waiting for its user, a CI alert, a terminal's own notification.
//! Oldest first; the view lists the newest on top.
//!
//! A sticky notice stays until it is dismissed or its cause resolves it by
//! its key; any other notice also goes by itself after
//! [`TRANSIENT_LIFETIME`].

// The browser editor reaches no runner, so nothing raises a notice there.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use std::time::{Duration, SystemTime};

use crate::message::NoticeTarget;

/// The most notices kept; the oldest is dropped beyond it, so a failure that
/// repeats cannot grow the list without bound.
pub const MAX_NOTICES: usize = 20;

/// How long a notice that is not sticky stays.
pub const TRANSIENT_LIFETIME: Duration = Duration::from_secs(60);

/// How a notice is coloured. Declared most severe first, so the smallest is
/// the most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// One box of the stack.
#[derive(Debug, Clone)]
pub struct Notice {
    pub id: u64,
    /// The cause's name: a notice raised again under it is updated in place,
    /// and the cause resolves it by it.
    pub key: Option<String>,
    pub severity: Severity,
    pub title: String,
    pub body: String,
    pub raised: SystemTime,
    pub sticky: bool,
    /// What a click on the title opens.
    pub open: Option<NoticeTarget>,
}

/// The notices shown, oldest first.
#[derive(Debug, Default)]
pub struct Notices {
    items: Vec<Notice>,
    next_id: u64,
}

impl Notices {
    /// The notices, oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Notice> {
        self.items.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn get(&self, id: u64) -> Option<&Notice> {
        self.items.iter().find(|n| n.id == id)
    }

    /// Raises a notice and returns its id. One with the same `key` is
    /// updated in place (its text, severity and time; its id stays); without
    /// a key, the same severity, title and body move to the bottom instead of
    /// showing twice.
    #[allow(clippy::too_many_arguments)]
    pub fn raise(
        &mut self,
        key: Option<String>,
        severity: Severity,
        title: impl Into<String>,
        body: impl Into<String>,
        sticky: bool,
        open: Option<NoticeTarget>,
        now: SystemTime,
    ) -> u64 {
        let (title, body) = (title.into(), body.into());
        if let Some(key) = &key
            && let Some(notice) = self.items.iter_mut().find(|n| n.key.as_ref() == Some(key))
        {
            notice.severity = severity;
            notice.title = title;
            notice.body = body;
            notice.raised = now;
            notice.sticky = sticky;
            notice.open = open;
            return notice.id;
        }
        if key.is_none() {
            self.items.retain(|n| {
                !(n.key.is_none() && n.severity == severity && n.title == title && n.body == body)
            });
        }
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(Notice {
            id,
            key,
            severity,
            title,
            body,
            raised: now,
            sticky,
            open,
        });
        while self.items.len() > MAX_NOTICES {
            self.items.remove(0);
        }
        id
    }

    pub fn dismiss(&mut self, id: u64) {
        self.items.retain(|n| n.id != id);
    }

    pub fn dismiss_all(&mut self) {
        self.items.clear();
    }

    /// Removes the notice its cause raised under `key`: the cause is gone.
    pub fn resolve(&mut self, key: &str) {
        self.items.retain(|n| n.key.as_deref() != Some(key));
    }

    /// Removes the notices that are not sticky and older than
    /// [`TRANSIENT_LIFETIME`]; returns whether any went.
    pub fn expire(&mut self, now: SystemTime) -> bool {
        let before = self.items.len();
        self.items.retain(|n| {
            n.sticky
                || now
                    .duration_since(n.raised)
                    .is_ok_and(|age| age < TRANSIENT_LIFETIME)
        });
        self.items.len() != before
    }

    /// The most severe notice's severity.
    pub fn worst(&self) -> Option<Severity> {
        self.items.iter().map(|n| n.severity).min()
    }
}

/// How long ago `raised` was, as a glance reads it: `now`, `5m`, `3h`, `2d`.
pub fn age(raised: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(raised).map_or(0, |d| d.as_secs());
    match secs {
        0..60 => "now".to_owned(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + secs)
    }

    fn texts(n: &Notices) -> Vec<(&str, &str)> {
        n.iter()
            .map(|n| (n.title.as_str(), n.body.as_str()))
            .collect()
    }

    #[test]
    fn a_keyed_notice_is_updated_in_place_and_keeps_its_id() {
        let mut n = Notices::default();
        let first = n.raise(
            Some("agent:a:1".into()),
            Severity::Warning,
            "asks",
            "Which?",
            true,
            None,
            at(0),
        );
        n.raise(None, Severity::Info, "other", "", false, None, at(1));
        let again = n.raise(
            Some("agent:a:1".into()),
            Severity::Error,
            "failed",
            "boom",
            true,
            None,
            at(5),
        );
        assert_eq!(first, again);
        assert_eq!(texts(&n), [("failed", "boom"), ("other", "")]);
        let notice = n.get(first).unwrap();
        assert_eq!((notice.severity, notice.raised), (Severity::Error, at(5)));
    }

    #[test]
    fn an_unkeyed_repeat_moves_to_the_bottom_instead_of_showing_twice() {
        let mut n = Notices::default();
        n.raise(None, Severity::Info, "a", "x", false, None, at(0));
        n.raise(None, Severity::Info, "b", "", false, None, at(1));
        n.raise(None, Severity::Info, "a", "x", false, None, at(2));
        assert_eq!(texts(&n), [("b", ""), ("a", "x")]);
        // Another severity is another notice.
        n.raise(None, Severity::Error, "a", "x", false, None, at(3));
        assert_eq!(n.len(), 3);
    }

    #[test]
    fn the_oldest_goes_beyond_the_limit() {
        let mut n = Notices::default();
        for i in 0..=MAX_NOTICES {
            n.raise(None, Severity::Info, format!("{i}"), "", true, None, at(0));
        }
        assert_eq!(n.len(), MAX_NOTICES);
        assert_eq!(n.iter().next().unwrap().title, "1");
    }

    #[test]
    fn resolving_a_key_removes_only_its_notice() {
        let mut n = Notices::default();
        n.raise(
            Some("alert:x".into()),
            Severity::Error,
            "a",
            "",
            true,
            None,
            at(0),
        );
        n.raise(
            Some("agent:y".into()),
            Severity::Warning,
            "b",
            "",
            true,
            None,
            at(0),
        );
        n.resolve("alert:x");
        assert_eq!(texts(&n), [("b", "")]);
        n.resolve("agent:y");
        assert!(n.is_empty());
    }

    #[test]
    fn a_transient_notice_expires_after_its_lifetime_and_a_sticky_one_stays() {
        let mut n = Notices::default();
        n.raise(None, Severity::Info, "transient", "", false, None, at(0));
        n.raise(None, Severity::Warning, "sticky", "", true, None, at(0));
        assert!(!n.expire(at(59)));
        assert_eq!(n.len(), 2);
        assert!(n.expire(at(60)));
        assert_eq!(texts(&n), [("sticky", "")]);
        assert!(!n.expire(at(86_400)));
        assert_eq!(n.worst(), Some(Severity::Warning));
    }

    #[test]
    fn the_worst_is_the_most_severe() {
        let mut n = Notices::default();
        assert_eq!(n.worst(), None);
        n.raise(None, Severity::Info, "i", "", false, None, at(0));
        n.raise(None, Severity::Error, "e", "", false, None, at(0));
        n.raise(None, Severity::Warning, "w", "", false, None, at(0));
        assert_eq!(n.worst(), Some(Severity::Error));
    }

    #[test]
    fn the_age_reads_at_a_glance() {
        assert_eq!(age(at(0), at(59)), "now");
        assert_eq!(age(at(0), at(60)), "1m");
        assert_eq!(age(at(0), at(3599)), "59m");
        assert_eq!(age(at(0), at(3600)), "1h");
        assert_eq!(age(at(0), at(86_399)), "23h");
        assert_eq!(age(at(0), at(86_400)), "1d");
        // A clock that went back reads as now.
        assert_eq!(age(at(10), at(0)), "now");
    }
}
