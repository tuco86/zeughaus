//! Runner labels and the selectors that pick runners by them.
//!
//! A label is `key` or `key=value`: the key `[a-z0-9][a-z0-9-]*`, the value
//! `[A-Za-z0-9._-]+`. The CI gives every runner `runner=<name>`, `os=<os>`
//! and `arch=<arch>`; `ci.toml` adds free ones. A selector is comma-separated
//! terms of the same form; a plan's `Runs on` is one selector per machine
//! the work needs, the first hosting the session.

use std::fmt;

/// The most machines one session's `Runs on` may ask for.
pub const MAX_MACHINES: usize = 4;

/// Comma-separated `key` or `key=value` terms, every one of which a runner's
/// labels must satisfy. The empty selector matches every runner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selector(Vec<(String, Option<String>)>);

impl Selector {
    pub fn parse(text: &str) -> Result<Selector, String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(Selector::default());
        }
        let mut terms = Vec::new();
        for term in trimmed.split(',').map(str::trim) {
            if !valid_label(term) {
                return Err(format!("selector {text}: {term:?} is not key or key=value"));
            }
            terms.push(match term.split_once('=') {
                Some((key, value)) => (key.to_owned(), Some(value.to_owned())),
                None => (term.to_owned(), None),
            });
        }
        Ok(Selector(terms))
    }

    /// Whether `labels` satisfy every term: a `key=value` term needs that
    /// label, a bare `key` the label `key` or any `key=<value>`.
    pub fn matches(&self, labels: &[String]) -> bool {
        self.0.iter().all(|(key, value)| {
            labels.iter().any(|label| {
                let (k, v) = match label.split_once('=') {
                    Some((k, v)) => (k, Some(v)),
                    None => (label.as_str(), None),
                };
                k == key
                    && match value {
                        Some(value) => v == Some(value.as_str()),
                        None => true,
                    }
            })
        })
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, (key, value)) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            f.write_str(key)?;
            if let Some(value) = value {
                write!(f, "={value}")?;
            }
        }
        Ok(())
    }
}

/// A `Runs on` list: one selector per machine, at most [`MAX_MACHINES`]; an
/// empty list is one machine that any runner can be.
pub fn parse_runs_on(list: &[String]) -> Result<Vec<Selector>, String> {
    if list.len() > MAX_MACHINES {
        return Err(format!("Runs on names at most {MAX_MACHINES} machines"));
    }
    if list.is_empty() {
        return Ok(vec![Selector::default()]);
    }
    list.iter().map(|text| Selector::parse(text)).collect()
}

/// Whether `label` is `key` or `key=value` by the rules above.
pub fn valid_label(label: &str) -> bool {
    let (key, value) = match label.split_once('=') {
        Some((key, value)) => (key, Some(value)),
        None => (label, None),
    };
    valid_key(key) && value.is_none_or(valid_value)
}

fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn valid_value(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_a_key_or_a_key_and_a_value() {
        for good in ["gpu", "os=windows", "cuda=12.4", "a-1=X_y.z-2"] {
            assert!(valid_label(good), "{good}");
        }
        for bad in ["", "Gpu", "-x", "os=", "=x", "os=a=b", "os=a b", "a_b"] {
            assert!(!valid_label(bad), "{bad}");
        }
    }

    #[test]
    fn a_selector_needs_every_term() {
        let labels: Vec<String> = ["os=macos", "arch=arm64", "gpu", "runner=atik"]
            .map(str::to_owned)
            .to_vec();
        let matches = |text: &str| Selector::parse(text).unwrap().matches(&labels);
        assert!(matches(""));
        assert!(matches("os=macos"));
        assert!(matches("os=macos, arch=arm64"));
        assert!(matches("os"));
        assert!(matches("gpu"));
        assert!(!matches("os=windows"));
        assert!(!matches("os=macos,cuda"));
        assert!(!matches("gpu=1"));
        assert_eq!(
            Selector::parse(" os=macos , gpu ").unwrap().to_string(),
            "os=macos,gpu"
        );
        let err = Selector::parse("os=mac os").unwrap_err();
        assert!(err.starts_with("selector os=mac os: "), "{err}");
        assert!(Selector::parse("os=,x").is_err());
    }

    #[test]
    fn runs_on_is_one_selector_per_machine() {
        assert_eq!(parse_runs_on(&[]).unwrap(), vec![Selector::default()]);
        let two = parse_runs_on(&["os=linux".to_owned(), "os=windows".to_owned()]).unwrap();
        assert_eq!(two.len(), 2);
        let five = vec!["gpu".to_owned(); 5];
        assert!(parse_runs_on(&five).is_err());
        assert!(parse_runs_on(&["Bad".to_owned()]).is_err());
    }
}
