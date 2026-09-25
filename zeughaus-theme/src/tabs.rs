//! The tab tree's catalog.
//!
//! The look is the tab crate's default resolved against the inner iced
//! theme: the bar sits on the chrome, a selected tab joins the content
//! background, an optional accent is drawn as a dot, a group's tabs sit on
//! its colour and the drop marker is the primary colour.

use iced::Color;
use iced_tabs::{Catalog, Placement, Status, Style, StyleFn};

use crate::Theme;

impl Catalog for Theme {
    type Class<'a> = StyleFn<'a, Self>;

    fn default<'a>() -> Self::Class<'a> {
        Box::new(|theme: &Self, status| iced_tabs::default(theme.base(), status))
    }

    fn style(&self, class: &Self::Class<'_>, status: Status) -> Style {
        class(self, status)
    }

    fn section_header(&self, placement: Placement) -> Style {
        iced_tabs::section_header(self.base(), placement)
    }

    fn group_header(&self, color: Color, marked: bool, placement: Placement) -> Style {
        iced_tabs::group_header(self.base(), color, marked, placement)
    }

    fn group_background(&self, color: Color, placement: Placement) -> Style {
        iced_tabs::group_background(self.base(), color, placement)
    }

    fn marker(&self) -> Color {
        iced_tabs::marker(self.base())
    }
}
