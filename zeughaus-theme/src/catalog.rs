//! The iced widget catalogs, every one of them forwarded to [`Theme::base`].
//!
//! Widgets are iced's to style: a pick list in this editor should look like a
//! pick list in any other iced program on the same theme. What this crate adds
//! sits beside them -- the graph, the terminal, the chrome -- so every catalog
//! here resolves iced's own class against the inner theme and returns what
//! iced would have returned.
//!
//! The inner class is built once, when the widget asks for its default class,
//! and captured: resolving a style must not allocate, it happens per widget
//! per frame.

use iced::widget::overlay::menu;
use iced::widget::{
    button, checkbox, container, pane_grid, pick_list, rule, scrollable, text, text_input,
};
// `iced::widget::svg` exists only when iced was built with its own `svg`
// feature, which this crate's `svg` feature turns on.
#[cfg(feature = "svg")]
use iced::widget::svg;

use crate::Theme;

/// A catalog whose style depends on a status.
macro_rules! forward_with_status {
    ($module:ident) => {
        impl $module::Catalog for Theme {
            type Class<'a> = $module::StyleFn<'a, Self>;

            fn default<'a>() -> Self::Class<'a> {
                let inner = <iced::Theme as $module::Catalog>::default();

                Box::new(move |theme: &Self, status| {
                    <iced::Theme as $module::Catalog>::style(theme.base(), &inner, status)
                })
            }

            fn style(&self, class: &Self::Class<'_>, status: $module::Status) -> $module::Style {
                class(self, status)
            }
        }
    };
}

/// A catalog whose style depends on nothing but the theme.
macro_rules! forward {
    ($module:ident) => {
        impl $module::Catalog for Theme {
            type Class<'a> = $module::StyleFn<'a, Self>;

            fn default<'a>() -> Self::Class<'a> {
                let inner = <iced::Theme as $module::Catalog>::default();

                Box::new(move |theme: &Self| {
                    <iced::Theme as $module::Catalog>::style(theme.base(), &inner)
                })
            }

            fn style(&self, class: &Self::Class<'_>) -> $module::Style {
                class(self)
            }
        }
    };
}

forward_with_status!(button);
forward_with_status!(checkbox);
forward_with_status!(scrollable);
forward_with_status!(text_input);
forward!(container);
forward!(rule);
forward!(text);

// A pick list is two things: the button-like field and the menu it drops. The
// trait pair mirrors that, and both halves qualify their `Class` because the
// supertrait has one too.
impl menu::Catalog for Theme {
    type Class<'a> = menu::StyleFn<'a, Self>;

    fn default<'a>() -> <Self as menu::Catalog>::Class<'a> {
        let inner = <iced::Theme as menu::Catalog>::default();

        Box::new(move |theme: &Self| <iced::Theme as menu::Catalog>::style(theme.base(), &inner))
    }

    fn style(&self, class: &<Self as menu::Catalog>::Class<'_>) -> menu::Style {
        class(self)
    }
}

impl pick_list::Catalog for Theme {
    type Class<'a> = pick_list::StyleFn<'a, Self>;

    fn default<'a>() -> <Self as pick_list::Catalog>::Class<'a> {
        let inner = <iced::Theme as pick_list::Catalog>::default();

        Box::new(move |theme: &Self, status| {
            <iced::Theme as pick_list::Catalog>::style(theme.base(), &inner, status)
        })
    }

    fn style(
        &self,
        class: &<Self as pick_list::Catalog>::Class<'_>,
        status: pick_list::Status,
    ) -> pick_list::Style {
        class(self, status)
    }
}

impl pane_grid::Catalog for Theme {
    type Class<'a> = pane_grid::StyleFn<'a, Self>;

    fn default<'a>() -> <Self as pane_grid::Catalog>::Class<'a> {
        let inner = <iced::Theme as pane_grid::Catalog>::default();

        Box::new(move |theme: &Self| {
            <iced::Theme as pane_grid::Catalog>::style(theme.base(), &inner)
        })
    }

    fn style(&self, class: &<Self as pane_grid::Catalog>::Class<'_>) -> pane_grid::Style {
        class(self)
    }
}

#[cfg(feature = "svg")]
forward_with_status!(svg);

#[cfg(test)]
mod tests {
    use iced::widget::button;

    use super::*;

    #[test]
    fn a_widget_style_is_iceds_own() {
        let theme = &Theme::pack()[2];

        let class = <Theme as button::Catalog>::default();
        let mine = <Theme as button::Catalog>::style(theme, &class, button::Status::Active);

        let inner = <iced::Theme as button::Catalog>::default();
        let iceds =
            <iced::Theme as button::Catalog>::style(theme.base(), &inner, button::Status::Active);

        assert_eq!(mine.background, iceds.background);
        assert_eq!(mine.text_color, iceds.text_color);
    }
}
