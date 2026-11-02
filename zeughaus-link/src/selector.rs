//! Runner labels and the selectors that pick runners by them.
//!
//! A label is `key` or `key=value`: the key `[a-z0-9][a-z0-9-]*`, the value
//! `[A-Za-z0-9._-]+`. The CI gives every runner `runner=<name>`, `os=<os>`
//! and `arch=<arch>`; `ci.toml` adds free ones.

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
}
