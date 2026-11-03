//! Thin scrollbars that stay out of the way: a 4 px thumb without a track,
//! shown only while the cursor rests on the bar or the view scrolls, then
//! faded out.
//!
//! [`vertical`] and [`horizontal`] wrap iced's own [`Scrollable`], which keeps
//! its geometry, hit testing, dragging and status colours. The wrapper only
//! decides, per frame, how opaque the thumb is.

use std::cell::Cell;
use std::rc::Rc;

use iced_widget::core::time::{Duration, Instant};
use iced_widget::core::widget::{self, Operation, Tree, Widget, tree};
use iced_widget::core::window::{self, RedrawRequest};
use iced_widget::core::{
    Background, Clipboard, Color, Event, Length, Pixels, Rectangle, Shell, Size, Vector,
    layout::{self, Layout},
    mouse, overlay, renderer,
};
use iced_widget::scrollable::{self, Direction, Scrollable, Scrollbar};

use crate::Element;

/// Width of the thumb; there is no rail around it.
pub const THICKNESS: f32 = 4.0;

/// Room on each side of the thumb that still counts as the bar, so a thin
/// thumb stays easy to hit.
const MARGIN: f32 = 2.0;

/// The strip at the end edge where iced hit-tests the bar: the same formula
/// as iced's own (`width.max(scroller_width) + 2 * margin`).
const ZONE: f32 = THICKNESS + 2.0 * MARGIN;

/// How long the cursor rests on the bar before it shows, so that crossing
/// it on the way elsewhere shows nothing.
const HOVER_DELAY: Duration = Duration::from_millis(400);

/// How long the bar stays after the last moment it was wanted.
const HOLD: Duration = Duration::from_millis(1000);

/// How long the bar takes to fade out after the hold.
const FADE: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Vertical,
    Horizontal,
}

fn bar() -> Scrollbar {
    Scrollbar::new()
        .width(THICKNESS)
        .scroller_width(THICKNESS)
        .margin(MARGIN)
}

/// A scrollable with a thin, self-hiding bar, configured before it becomes
/// an element.
pub struct Scroll<'a, Message, Theme>
where
    Theme: scrollable::Catalog,
{
    inner: Scrollable<'a, Message, Theme>,
    axis: Axis,
}

/// A vertically scrolling view of `content`.
pub fn vertical<'a, Message, Theme>(
    content: impl Into<Element<'a, Message, Theme>>,
) -> Scroll<'a, Message, Theme>
where
    Theme: scrollable::Catalog,
{
    Scroll {
        inner: Scrollable::with_direction(content, Direction::Vertical(bar())),
        axis: Axis::Vertical,
    }
}

/// A horizontally scrolling view of `content`.
pub fn horizontal<'a, Message, Theme>(
    content: impl Into<Element<'a, Message, Theme>>,
) -> Scroll<'a, Message, Theme>
where
    Theme: scrollable::Catalog,
{
    Scroll {
        inner: Scrollable::with_direction(content, Direction::Horizontal(bar())),
        axis: Axis::Horizontal,
    }
}

impl<Message, Theme: scrollable::Catalog> Scroll<'_, Message, Theme> {
    /// The id widget operations find the scrollable by.
    pub fn id(mut self, id: impl Into<widget::Id>) -> Self {
        self.inner = self.inner.id(id);
        self
    }

    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.inner = self.inner.width(width);
        self
    }

    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.inner = self.inner.height(height);
        self
    }

    /// Embeds the bar beside the content with `spacing` between them: it
    /// takes layout room and never covers the content.
    pub fn spacing(mut self, spacing: impl Into<Pixels>) -> Self {
        self.inner = self.inner.spacing(spacing);
        self
    }
}

impl<'a, Message, Theme> From<Scroll<'a, Message, Theme>> for Element<'a, Message, Theme>
where
    Message: 'a,
    Theme: scrollable::Catalog + 'a,
    <Theme as scrollable::Catalog>::Class<'a>: From<scrollable::StyleFn<'a, Theme>>,
{
    fn from(scroll: Scroll<'a, Message, Theme>) -> Self {
        let opacity = Rc::new(Cell::new(0.0_f32));
        let shared = Rc::clone(&opacity);
        let base = <Theme as scrollable::Catalog>::default();
        let inner = scroll.inner.style(move |theme: &Theme, status| {
            faded(
                <Theme as scrollable::Catalog>::style(theme, &base, status),
                shared.get(),
            )
        });
        Element::new(Fade {
            content: inner.into(),
            axis: scroll.axis,
            opacity,
        })
    }
}

/// `style` without a track and with its thumb at `opacity`; a hidden thumb
/// is exactly transparent, which makes iced skip its quad.
fn faded(mut style: scrollable::Style, opacity: f32) -> scrollable::Style {
    for rail in [&mut style.vertical_rail, &mut style.horizontal_rail] {
        rail.background = None;
        rail.border.color = Color::TRANSPARENT;
        if opacity <= 0.0 {
            rail.scroller.background = Background::Color(Color::TRANSPARENT);
            rail.scroller.border.color = Color::TRANSPARENT;
        } else {
            rail.scroller.background = rail.scroller.background.scale_alpha(opacity);
            rail.scroller.border.color = rail.scroller.border.color.scale_alpha(opacity);
        }
    }
    style
}

/// Where the bar of a scrollable with `bounds` around `content` lies, if
/// the content overflows along `axis`. An embedded bar sits in the same
/// strip: iced puts it at the end edge of the whole bounds.
fn zone(axis: Axis, bounds: Rectangle, content: Rectangle) -> Option<Rectangle> {
    match axis {
        Axis::Vertical => (content.height > bounds.height).then_some(Rectangle {
            x: bounds.x + bounds.width - ZONE,
            y: bounds.y,
            width: ZONE,
            height: bounds.height,
        }),
        Axis::Horizontal => (content.width > bounds.width).then_some(Rectangle {
            x: bounds.x,
            y: bounds.y + bounds.height - ZONE,
            width: bounds.width,
            height: ZONE,
        }),
    }
}

/// The thumb's opacity `since_wanted` after the bar was last wanted.
fn opacity(since_wanted: Duration) -> f32 {
    if since_wanted <= HOLD {
        1.0
    } else {
        (1.0 - (since_wanted - HOLD).as_secs_f32() / FADE.as_secs_f32()).max(0.0)
    }
}

/// When the bar is wanted, and so how opaque it is. Every method takes the
/// time it acts at; none reads a clock.
#[derive(Debug, Default)]
struct Visibility {
    /// Since when the cursor rests on the bar.
    hover_since: Option<Instant>,
    /// A left press that started on the bar is held.
    grabbed: bool,
    /// The last moment the bar was wanted.
    wanted_at: Option<Instant>,
    /// The scroll offset of the last frame.
    translation: Option<Vector>,
    /// What the next draw uses.
    opacity: f32,
}

impl Visibility {
    fn hover(&mut self, over: bool, now: Instant) -> RedrawRequest {
        if over {
            let since = *self.hover_since.get_or_insert(now);
            if now < since + HOVER_DELAY {
                RedrawRequest::At(since + HOVER_DELAY)
            } else {
                RedrawRequest::Wait
            }
        } else {
            match self.hover_since.take() {
                Some(since) if now >= since + HOVER_DELAY => {
                    self.wanted_at = Some(now);
                    RedrawRequest::At(now + HOLD)
                }
                _ => RedrawRequest::Wait,
            }
        }
    }

    fn press(&mut self, over: bool) {
        self.grabbed = over;
    }

    fn release(&mut self, now: Instant) -> RedrawRequest {
        if self.grabbed {
            self.grabbed = false;
            self.wanted_at = Some(now);
            RedrawRequest::At(now + HOLD)
        } else {
            RedrawRequest::Wait
        }
    }

    fn frame(&mut self, now: Instant, translation: Vector) -> RedrawRequest {
        if self.translation.is_some_and(|last| last != translation) {
            self.wanted_at = Some(now);
        }
        self.translation = Some(translation);
        let hover_pending = self.hover_since.filter(|since| now < *since + HOVER_DELAY);
        if self.grabbed || (self.hover_since.is_some() && hover_pending.is_none()) {
            self.wanted_at = Some(now);
        }
        let elapsed = self.wanted_at.map(|at| now.saturating_duration_since(at));
        self.opacity = elapsed.map_or(0.0, opacity);

        let mut request = RedrawRequest::Wait;
        if let Some(since) = hover_pending {
            request = request.min(RedrawRequest::At(since + HOVER_DELAY));
        }
        if let (Some(at), Some(elapsed)) = (self.wanted_at, elapsed) {
            if elapsed < HOLD {
                request = request.min(RedrawRequest::At(at + HOLD));
            } else if elapsed < HOLD + FADE {
                request = request.min(RedrawRequest::NextFrame);
            }
        }
        request
    }
}

/// Reads the scroll offset of the scrollable it is run on, without visiting
/// the content below it.
struct Probe(Option<Vector>);

impl Operation for Probe {
    fn traverse(&mut self, _operate: &mut dyn FnMut(&mut dyn Operation)) {}

    fn scrollable(
        &mut self,
        _id: Option<&widget::Id>,
        _bounds: Rectangle,
        _content_bounds: Rectangle,
        translation: Vector,
        _state: &mut dyn widget::operation::Scrollable,
    ) {
        let _ = self.0.get_or_insert(translation);
    }
}

/// The scrollable with the thumb's opacity decided from the cursor, presses
/// and the scroll offset; its style reads `opacity` when it draws.
struct Fade<'a, Message, Theme> {
    content: Element<'a, Message, Theme>,
    axis: Axis,
    opacity: Rc<Cell<f32>>,
}

impl<Message, Theme> Widget<Message, Theme, iced_widget::Renderer> for Fade<'_, Message, Theme> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Visibility>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Visibility::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced_widget::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced_widget::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced_widget::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        let bounds = layout.bounds();
        let content = layout.children().next().map_or(bounds, |c| c.bounds());
        let over = zone(self.axis, bounds, content)
            .is_some_and(|zone| cursor.position_over(zone).is_some());

        let request = match event {
            Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft) => tree
                .state
                .downcast_mut::<Visibility>()
                .hover(over, Instant::now()),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                tree.state.downcast_mut::<Visibility>().press(over);
                RedrawRequest::Wait
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => tree
                .state
                .downcast_mut::<Visibility>()
                .release(Instant::now()),
            Event::Window(window::Event::RedrawRequested(now)) => {
                let hover = tree.state.downcast_mut::<Visibility>().hover(over, *now);
                let mut probe = Probe(None);
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut probe,
                );
                let frame = tree
                    .state
                    .downcast_mut::<Visibility>()
                    .frame(*now, probe.0.unwrap_or(Vector::ZERO));
                hover.min(frame)
            }
            _ => RedrawRequest::Wait,
        };
        shell.request_redraw_at(request);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced_widget::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced_widget::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.opacity
            .set(tree.state.downcast_ref::<Visibility>().opacity);
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced_widget::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, iced_widget::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Vector = Vector::ZERO;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_short_hover_shows_nothing_and_400ms_shows_the_bar() {
        let t0 = Instant::now();
        let mut v = Visibility::default();
        assert_eq!(v.hover(true, t0), RedrawRequest::At(t0 + ms(400)));
        v.frame(t0 + ms(399), T);
        assert_eq!(v.opacity, 0.0);
        v.frame(t0 + ms(400), T);
        assert_eq!(v.opacity, 1.0);
    }

    #[test]
    fn scrolling_shows_the_bar_at_once_then_fades_after_the_hold() {
        let t0 = Instant::now();
        let scrolled = Vector::new(0.0, 30.0);
        let mut v = Visibility::default();
        v.frame(t0, T);
        assert_eq!(v.opacity, 0.0);
        let t1 = t0 + ms(10);
        v.frame(t1, scrolled);
        assert_eq!(v.opacity, 1.0);
        v.frame(t1 + HOLD, scrolled);
        assert_eq!(v.opacity, 1.0);
        assert_eq!(
            v.frame(t1 + HOLD + FADE / 2, scrolled),
            RedrawRequest::NextFrame
        );
        assert!((v.opacity - 0.5).abs() < 0.01, "{}", v.opacity);
        assert_eq!(v.frame(t1 + HOLD + FADE, scrolled), RedrawRequest::Wait);
        assert_eq!(v.opacity, 0.0);
    }

    #[test]
    fn leaving_a_shown_bar_holds_it_before_fading() {
        let t0 = Instant::now();
        let mut v = Visibility::default();
        v.hover(true, t0);
        v.frame(t0 + ms(500), T);
        assert_eq!(v.opacity, 1.0);
        let left = t0 + ms(2000);
        assert_eq!(v.hover(false, left), RedrawRequest::At(left + HOLD));
        v.frame(left + HOLD - ms(1), T);
        assert_eq!(v.opacity, 1.0);
        v.frame(left + HOLD + FADE, T);
        assert_eq!(v.opacity, 0.0);
    }

    #[test]
    fn a_held_thumb_stays_while_the_cursor_is_off_the_bar() {
        let t0 = Instant::now();
        let mut v = Visibility::default();
        v.hover(true, t0);
        v.press(true);
        v.hover(false, t0 + ms(50));
        let later = t0 + ms(5000);
        v.frame(later, T);
        assert_eq!(v.opacity, 1.0);
        v.release(later);
        v.frame(later + HOLD + FADE, T);
        assert_eq!(v.opacity, 0.0);
    }

    #[test]
    fn a_hidden_bar_draws_neither_thumb_nor_track() {
        let base = scrollable::default(
            &iced_widget::Theme::Dark,
            scrollable::Status::Active {
                is_horizontal_scrollbar_disabled: false,
                is_vertical_scrollbar_disabled: false,
            },
        );
        let hidden = faded(base, 0.0);
        assert_eq!(hidden.vertical_rail.background, None);
        assert_eq!(
            hidden.vertical_rail.scroller.background,
            Background::Color(Color::TRANSPARENT)
        );
        let shown = faded(base, 1.0);
        assert_eq!(shown.vertical_rail.background, None);
        assert_eq!(
            shown.vertical_rail.scroller.background,
            base.vertical_rail.scroller.background
        );
    }

    #[test]
    fn a_bar_zone_exists_only_where_the_content_overflows() {
        let bounds = Rectangle::new(iced_widget::core::Point::ORIGIN, Size::new(100.0, 100.0));
        let content = |w, h| Rectangle::new(iced_widget::core::Point::ORIGIN, Size::new(w, h));
        assert_eq!(zone(Axis::Vertical, bounds, content(100.0, 100.0)), None);
        assert_eq!(
            zone(Axis::Vertical, bounds, content(100.0, 300.0)),
            Some(Rectangle {
                x: 92.0,
                y: 0.0,
                width: 8.0,
                height: 100.0
            })
        );
        assert_eq!(
            zone(Axis::Horizontal, bounds, content(300.0, 100.0)),
            Some(Rectangle {
                x: 0.0,
                y: 92.0,
                width: 100.0,
                height: 8.0
            })
        );
    }
}
