//! The one shape of a name the CI hands out and the runner accepts: runner,
//! machine, repository, job, image, workspace and channel names. They become
//! path components and container names, so the shape is kept narrow on
//! purpose.

/// `[a-z0-9][a-z0-9-]*`
pub fn valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}
