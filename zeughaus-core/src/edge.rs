#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EdgeSemantic {
    #[default]
    LastValue,
    BoundedQueue(usize),
    Queue,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_last_value() {
        assert_eq!(EdgeSemantic::default(), EdgeSemantic::LastValue);
    }
}
