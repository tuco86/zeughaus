//! The tab bar's catalog.
//!
//! A tab is a button in disguise, so the look is the tab crate's own default
//! resolved against the inner iced theme: the selected tab wears the primary
//! pair, the others are transparent with the theme's text, and a tab with an
//! accent (a job, a terminal that reported one) borders itself in it.

use iced_tabs::{Catalog, Status, Style, StyleFn};

use crate::Theme;

impl Catalog for Theme {
    type Class<'a> = StyleFn<'a, Self>;

    fn default<'a>() -> Self::Class<'a> {
        Box::new(|theme: &Self, status| iced_tabs::default(theme.base(), status))
    }

    fn style(&self, class: &Self::Class<'_>, status: Status) -> Style {
        class(self, status)
    }
}
