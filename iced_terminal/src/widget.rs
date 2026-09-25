//! The widget: one terminal surface, one primitive, and the input it turns
//! into [`TerminalCommand`]s.
//!
//! The widget owns no terminal state. It reads a
//! [`zeughaus_mux::view::TerminalView`] -- the client's copy of what the
//! runner has -- through a [`SharedView`] handle whose writer is the
//! transport task, and reports what the user did through [`Action`]; the
//! caller decides what to send and what to apply. That split is what lets a
//! second client view the same terminal: a viewer renders identically and its
//! keystrokes simply never become commands.
//!
//! Three rules the rest of this file exists to keep:
//!
//! - Only the controlling client emits input. A viewer may select, copy and
//!   scroll, all of which are local, and may ask for the lease with one
//!   explicit shortcut.
//! - A resize costs the child a `SIGWINCH` and a redraw, so it is emitted only
//!   when the *integer* cell geometry changes, and only after it stopped
//!   changing -- dragging a split otherwise sends one per frame.
//! - An idle terminal schedules nothing. The only timer is the cursor blink,
//!   and it only runs while this pane has the keyboard.
//! - The shared view is locked only for as long as it takes to copy out what
//!   one event or one frame needs, and never across a `publish`: the
//!   application's message handler is free to lock the very same mutex.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::advanced::input_method::{self, InputMethod};
use iced::advanced::layout::{self, Layout};
use iced::advanced::mouse;
use iced::advanced::renderer;
use iced::advanced::widget::{Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::event::Event;
use iced::{Element, Length, Point, Rectangle, Size, keyboard, window};

use zeughaus_mux::input::MAX_TEXT_BYTES;
use zeughaus_mux::view::TerminalView;
use zeughaus_mux::{
    CellSpan, CellStyle, Dimensions, MouseInput, MouseKind, StyleFlags, TerminalCommand, Underline,
};

use crate::cache::{palette_generation, row_key};
use crate::font;
use crate::geometry::{cell_at, grid_size};
use crate::input;
use crate::pipeline::{CursorSpec, Frame, FrameRow, Highlight, TerminalPrimitive};
use crate::selection::{self, GridPoint, Mode, Selection};
use crate::style::{Catalog, Style, StyleFn};

/// Default text size, in logical pixels.
const DEFAULT_FONT_SIZE: f32 = 14.0;
/// How long the cell geometry has to hold still before a resize is sent.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(60);
/// Half a blink period.
const BLINK_INTERVAL: Duration = Duration::from_millis(500);
/// Two clicks within this are a double click.
const MULTI_CLICK: Duration = Duration::from_millis(400);
/// Lines one wheel notch scrolls when the child is not reading the mouse.
const WHEEL_LINES: i64 = 3;

/// What the user did. Everything that leaves this widget leaves as one of
/// these; the caller owns the transport, the clipboard and the view.
#[derive(Debug, Clone)]
pub enum Action {
    /// Send this to the runner. `Key`/`Text`/`Paste`/`Mouse` already carry
    /// the serial handed in through [`Terminal::next_serial`].
    Command(TerminalCommand),
    /// Scroll the view by this many rows (negative is up, into history), then
    /// send a fresh `Viewport`. The widget does not do it itself: the shared
    /// view has exactly one writer, and it is not the widget.
    ScrollBy(i64),
    /// The selected text, for the clipboard.
    Copy(String),
    /// The user explicitly activated a hyperlink. Nothing is opened here.
    OpenLink(String),
    /// The user asked for the control lease.
    TakeControl,
    /// The pane gained or lost keyboard focus.
    Focused(bool),
}

/// The terminal state a pane shows: written by the transport task that
/// receives heads and deltas, read by the widget under a short lock. `None`
/// until the first head arrived.
pub type SharedView = Arc<Mutex<Option<TerminalView>>>;

/// A terminal surface.
pub struct Terminal<'a, Message, Theme = iced::Theme>
where
    Theme: Catalog,
{
    view: SharedView,
    id: u64,
    on_action: Option<Box<dyn Fn(Action) -> Message + 'a>>,
    controlling: bool,
    focused: bool,
    /// Keys the application keeps even while this pane has the keyboard.
    reserved: Option<fn(&keyboard::Key, keyboard::Modifiers) -> bool>,
    next_serial: u64,
    font_size: f32,
    class: Theme::Class<'a>,
}

impl<'a, Message, Theme> Terminal<'a, Message, Theme>
where
    Theme: Catalog,
{
    /// A terminal drawing whatever `view` holds. `id` must be stable for the
    /// surface: the renderer keeps this pane's GPU buffers under it.
    pub fn new(view: SharedView, id: u64) -> Self {
        Terminal {
            view,
            id,
            on_action: None,
            controlling: false,
            focused: false,
            reserved: None,
            next_serial: 1,
            font_size: DEFAULT_FONT_SIZE,
            class: Theme::default(),
        }
    }

    pub fn on_action(mut self, f: impl Fn(Action) -> Message + 'a) -> Self {
        self.on_action = Some(Box::new(f));
        self
    }

    /// Whether this client holds the control lease. Only then do keys, mouse
    /// reports and resizes travel.
    pub fn controlling(mut self, controlling: bool) -> Self {
        self.controlling = controlling;
        self
    }

    /// Whether this pane has the keyboard. The application tracks focus, not
    /// the widget: a terminal must not steal it from a command palette.
    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// Keys that stay the application's while this pane has the keyboard:
    /// they neither reach the child nor are captured, so a shortcut such as
    /// the command palette's works from inside a shell too.
    pub fn reserved(mut self, reserved: fn(&keyboard::Key, keyboard::Modifiers) -> bool) -> Self {
        self.reserved = Some(reserved);
        self
    }

    /// The serial the next input command must carry. The widget counts up
    /// from here within a frame; the caller stores the last one it saw on an
    /// [`Action::Command`].
    pub fn next_serial(mut self, serial: u64) -> Self {
        self.next_serial = serial;
        self
    }

    pub fn font_size(mut self, font_size: f32) -> Self {
        self.font_size = font_size;
        self
    }

    /// The colours this surface falls back to, as a function of the theme.
    pub fn style(mut self, style: impl Fn(&Theme) -> Style + 'a) -> Self
    where
        Theme::Class<'a>: From<StyleFn<'a, Theme>>,
    {
        self.class = (Box::new(style) as StyleFn<'a, Theme>).into();
        self
    }

    /// The style class of this surface.
    pub fn class(mut self, class: impl Into<Theme::Class<'a>>) -> Self {
        self.class = class.into();
        self
    }
}

/// Everything the widget remembers between events.
#[derive(Debug)]
struct State {
    /// The cell geometry the runner was last told about.
    sent_grid: Dimensions,
    /// A geometry waiting out the debounce.
    pending_grid: Option<(Dimensions, Instant)>,
    selection: Option<Selection>,
    dragging: bool,
    modifiers: keyboard::Modifiers,
    pointer: Option<Point>,
    held: Option<mouse::Button>,
    hovered_link: Option<String>,
    blink_on: bool,
    blink_at: Option<Instant>,
    preedit: Option<String>,
    serial: u64,
    last_click: Option<(Instant, (u16, u16), u8)>,
}

impl Default for State {
    fn default() -> Self {
        State {
            // Zero is not a grid the wire accepts, so the first layout always
            // looks like a change and the runner learns the real size.
            sent_grid: Dimensions { cols: 0, rows: 0 },
            pending_grid: None,
            selection: None,
            dragging: false,
            modifiers: keyboard::Modifiers::empty(),
            pointer: None,
            held: None,
            hovered_link: None,
            blink_on: true,
            blink_at: None,
            preedit: None,
            serial: 1,
            last_click: None,
        }
    }
}

impl State {
    fn next_serial(&mut self) -> u64 {
        let serial = self.serial;
        self.serial = self.serial.saturating_add(1);
        serial
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for Terminal<'_, Message, Theme>
where
    Theme: Catalog,
    Renderer: iced_wgpu::primitive::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: Length::Fill,
            height: Length::Fill,
        }
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, Length::Fill, Length::Fill)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State>();
        let bounds = layout.bounds();
        let style = theme.style(&self.class);
        // The snapshot is built while the lock is held and the guard is gone
        // before the primitive is handed over: from here on the frame is the
        // renderer's own data.
        let frame = self
            .with_view(|view| self.frame(view, state, bounds.size(), &style))
            .unwrap_or_else(|| self.empty_frame(bounds.size(), &style));
        renderer.draw_primitive(
            bounds,
            TerminalPrimitive {
                id: self.id,
                frame: Arc::new(frame),
            },
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if !cursor.is_over(layout.bounds()) {
            return mouse::Interaction::None;
        }
        let state = tree.state.downcast_ref::<State>();
        let Some(mouse_reporting) = self.with_view(|view| view.modes.mouse_reporting) else {
            return mouse::Interaction::default();
        };
        if state.hovered_link.is_some() && state.modifiers.control() {
            mouse::Interaction::Pointer
        } else if mouse_reporting && self.controlling {
            mouse::Interaction::None
        } else {
            mouse::Interaction::Text
        }
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        state.serial = state.serial.max(self.next_serial);
        let bounds = layout.bounds();

        match event {
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.modifiers = *modifiers;
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modified_key,
                modifiers,
                text,
                ..
            }) => {
                state.modifiers = *modifiers;
                // The same key the application's shortcut matches: the one
                // before modifiers were applied.
                let reserved = self
                    .reserved
                    .is_some_and(|reserved| reserved(key, *modifiers));
                if self.focused && !reserved {
                    self.on_key(
                        state,
                        modified_key,
                        *modifiers,
                        text.as_deref(),
                        clipboard,
                        shell,
                    );
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(button)) => {
                if let Some(local) = cursor.position_in(bounds) {
                    self.on_press(state, *button, local, bounds.size(), shell);
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(button)) => {
                self.on_release(state, *button, cursor, bounds, shell);
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                self.on_move(state, cursor, bounds, shell);
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                if cursor.is_over(bounds) {
                    self.on_wheel(state, *delta, cursor, bounds, shell);
                }
            }
            Event::InputMethod(input_method::Event::Preedit(content, _)) => {
                if self.focused {
                    state.preedit = Some(content.clone());
                    shell.request_redraw();
                }
            }
            Event::InputMethod(input_method::Event::Commit(text)) => {
                if self.focused {
                    state.preedit = None;
                    if self.controlling && !text.is_empty() && text.len() <= MAX_TEXT_BYTES {
                        let serial = state.next_serial();
                        self.publish(
                            shell,
                            Action::Command(TerminalCommand::Text {
                                serial,
                                text: text.clone(),
                            }),
                        );
                    }
                    shell.request_redraw();
                }
            }
            Event::InputMethod(input_method::Event::Closed) => {
                if state.preedit.take().is_some() {
                    shell.request_redraw();
                }
            }
            Event::Window(window::Event::RedrawRequested(now)) => {
                self.on_redraw(state, *now, bounds, shell);
            }
            _ => {}
        }
    }
}

impl<Message, Theme> Terminal<'_, Message, Theme>
where
    Theme: Catalog,
{
    /// Reads the shared view under the shortest possible lock: the guard is
    /// dropped on the way out, so a caller can only ever publish, redraw or
    /// send after it let go. `None` means no head arrived yet.
    fn with_view<T>(&self, read: impl FnOnce(&TerminalView) -> T) -> Option<T> {
        // A panic in some other thread's message handler must not take the
        // interface down with it; the data behind the lock is still whole.
        let view = self
            .view
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        view.as_ref().map(read)
    }

    fn publish(&self, shell: &mut Shell<'_, Message>, action: Action) {
        if let Some(on_action) = &self.on_action {
            shell.publish(on_action(action));
        }
    }

    fn metrics(&self) -> crate::geometry::CellMetrics {
        font::cell_metrics(self.font_size)
    }

    fn grid(&self, size: Size) -> Dimensions {
        grid_size(size, self.metrics())
    }

    // ------------------------------------------------------------ input ---

    fn on_key(
        &self,
        state: &mut State,
        key: &keyboard::Key,
        modifiers: keyboard::Modifiers,
        text: Option<&str>,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) {
        // The application's own shortcuts must not fire while a shell has the
        // keyboard, so every key a focused terminal understands is captured
        // and `Ctrl+Shift+Escape` is the way out.
        if modifiers.control()
            && modifiers.shift()
            && let Some(action) = self.chord(state, key, clipboard)
        {
            self.publish(shell, action);
            shell.capture_event();
            shell.request_redraw();
            return;
        }

        // Everything below this point needs a live view: before the first
        // head there is nothing to scroll and no child to type into, and the
        // keys are still swallowed so the application's shortcuts stay quiet.
        let Some(rows) = self.with_view(|view| view.dimensions.rows.max(1)) else {
            shell.capture_event();
            return;
        };

        if modifiers.shift()
            && let keyboard::Key::Named(named) = key
        {
            let page = i64::from(rows);
            let lines = match named {
                keyboard::key::Named::PageUp => Some(-page),
                keyboard::key::Named::PageDown => Some(page),
                _ => None,
            };
            if let Some(lines) = lines {
                self.publish(shell, Action::ScrollBy(lines));
                shell.capture_event();
                return;
            }
        }

        if !self.controlling {
            // A viewer's keystrokes are dropped, not queued and not sent.
            shell.capture_event();
            return;
        }

        if let Some(input) = input::key_input(key, modifiers) {
            let serial = state.next_serial();
            self.publish(
                shell,
                Action::Command(TerminalCommand::Key { serial, input }),
            );
            shell.capture_event();
            return;
        }

        // A composition or a layout the key model has no name for still has
        // to reach the child: send what the platform says was typed.
        if let Some(text) = text.filter(|text| {
            !text.is_empty() && text.len() <= MAX_TEXT_BYTES && !text.chars().all(char::is_control)
        }) {
            let serial = state.next_serial();
            self.publish(
                shell,
                Action::Command(TerminalCommand::Text {
                    serial,
                    text: text.to_string(),
                }),
            );
            shell.capture_event();
        }
    }

    /// `Ctrl+Shift+...`: the shortcuts that are the widget's, not the child's.
    fn chord(
        &self,
        state: &mut State,
        key: &keyboard::Key,
        clipboard: &mut dyn Clipboard,
    ) -> Option<Action> {
        match key {
            keyboard::Key::Named(keyboard::key::Named::Escape) => Some(Action::Focused(false)),
            keyboard::Key::Character(character) => {
                let character = character.chars().next()?.to_ascii_lowercase();
                match character {
                    'c' => {
                        let selection = state.selection.as_ref()?;
                        let text = self.with_view(|view| selection::extract(view, selection))?;
                        (!text.is_empty()).then_some(Action::Copy(text))
                    }
                    'v' => {
                        if !self.controlling {
                            return None;
                        }
                        let text = clipboard.read(iced::advanced::clipboard::Kind::Standard)?;
                        if text.is_empty() || text.len() > MAX_TEXT_BYTES {
                            return None;
                        }
                        let serial = state.next_serial();
                        Some(Action::Command(TerminalCommand::Paste { serial, text }))
                    }
                    't' => Some(Action::TakeControl),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn on_press(
        &self,
        state: &mut State,
        button: mouse::Button,
        local: Point,
        size: Size,
        shell: &mut Shell<'_, Message>,
    ) {
        state.held = Some(button);
        if !self.focused {
            self.publish(shell, Action::Focused(true));
        }

        let grid = self.grid(size);
        let (col, row) = cell_at(local, self.metrics(), grid);

        // One lock for everything this press needs from the view. The link
        // is looked up here because it needs the same borrow, not because a
        // click on a link is decided early.
        let Some((mouse_reporting, stable, link)) = self.with_view(|view| {
            let stable = top_row(view) + i64::from(row);
            let link = (state.modifiers.control() && button == mouse::Button::Left)
                .then(|| selection::link_at(view, stable, col).map(str::to_string))
                .flatten();
            (view.modes.mouse_reporting, stable, link)
        }) else {
            return;
        };

        if mouse_reporting && self.controlling {
            if let Some(button) = input::mouse_button(button) {
                let serial = state.next_serial();
                self.publish(
                    shell,
                    Action::Command(TerminalCommand::Mouse {
                        serial,
                        input: MouseInput {
                            kind: MouseKind::Press,
                            button: Some(button),
                            col,
                            row,
                            modifiers: input::modifiers(state.modifiers),
                        },
                    }),
                );
            }
            shell.capture_event();
            return;
        }

        if button != mouse::Button::Left {
            return;
        }

        // A link is only ever followed on an explicit modified click, and
        // only reported: nothing here opens anything.
        if let Some(link) = link {
            self.publish(shell, Action::OpenLink(link));
            shell.capture_event();
            return;
        }

        let now = Instant::now();
        let clicks = match state.last_click {
            Some((at, cell, count))
                if cell == (col, row) && now.duration_since(at) < MULTI_CLICK =>
            {
                count % 3 + 1
            }
            _ => 1,
        };
        state.last_click = Some((now, (col, row), clicks));

        let mode = match clicks {
            2 => Mode::Word,
            3 => Mode::Line,
            _ => Mode::Char,
        };
        state.selection = Some(Selection::new(GridPoint::new(stable, col), mode));
        state.dragging = true;
        shell.capture_event();
        shell.request_redraw();
    }

    fn on_move(
        &self,
        state: &mut State,
        cursor: mouse::Cursor,
        bounds: Rectangle,
        shell: &mut Shell<'_, Message>,
    ) {
        let Some(local) = cursor.position_in(bounds) else {
            state.pointer = None;
            return;
        };
        state.pointer = Some(local);

        let grid = self.grid(bounds.size());
        let (col, row) = cell_at(local, self.metrics(), grid);
        let Some((mouse_reporting, stable, link)) = self.with_view(|view| {
            let stable = top_row(view) + i64::from(row);
            let link = selection::link_at(view, stable, col).map(str::to_string);
            (view.modes.mouse_reporting, stable, link)
        }) else {
            return;
        };

        if link != state.hovered_link {
            state.hovered_link = link;
            shell.request_redraw();
        }

        if mouse_reporting && self.controlling {
            if let Some(button) = state.held.and_then(input::mouse_button) {
                let serial = state.next_serial();
                self.publish(
                    shell,
                    Action::Command(TerminalCommand::Mouse {
                        serial,
                        input: MouseInput {
                            kind: MouseKind::Move,
                            button: Some(button),
                            col,
                            row,
                            modifiers: input::modifiers(state.modifiers),
                        },
                    }),
                );
            }
            return;
        }

        if state.dragging
            && let Some(selection) = &mut state.selection
        {
            let head = GridPoint::new(stable, col);
            if selection.head != head {
                selection.head = head;
                shell.request_redraw();
            }
        }
    }

    fn on_release(
        &self,
        state: &mut State,
        button: mouse::Button,
        cursor: mouse::Cursor,
        bounds: Rectangle,
        shell: &mut Shell<'_, Message>,
    ) {
        state.held = None;
        let was_dragging = std::mem::take(&mut state.dragging);
        // A click that never moved selected nothing: a single-cell highlight
        // left behind every click would read as a stray cursor. Word and
        // line selections are whole by construction and stay.
        if was_dragging
            && let Some(selection) = &state.selection
            && selection.mode == Mode::Char
            && selection.anchor == selection.head
        {
            state.selection = None;
            shell.request_redraw();
        }

        if self
            .with_view(|view| view.modes.mouse_reporting)
            .unwrap_or(false)
            && self.controlling
            && let Some(local) = cursor.position_in(bounds)
            && let Some(button) = input::mouse_button(button)
        {
            let grid = self.grid(bounds.size());
            let (col, row) = cell_at(local, self.metrics(), grid);
            let serial = state.next_serial();
            self.publish(
                shell,
                Action::Command(TerminalCommand::Mouse {
                    serial,
                    input: MouseInput {
                        kind: MouseKind::Release,
                        button: Some(button),
                        col,
                        row,
                        modifiers: input::modifiers(state.modifiers),
                    },
                }),
            );
        }
    }

    fn on_wheel(
        &self,
        state: &mut State,
        delta: mouse::ScrollDelta,
        cursor: mouse::Cursor,
        bounds: Rectangle,
        shell: &mut Shell<'_, Message>,
    ) {
        let metrics = self.metrics();
        let notches = match delta {
            mouse::ScrollDelta::Lines { y, .. } => y,
            mouse::ScrollDelta::Pixels { y, .. } => y / metrics.height.max(1.0),
        };
        if notches == 0.0 || !notches.is_finite() {
            return;
        }

        // No view is no viewport either: there is nothing to scroll yet.
        let Some(mouse_reporting) = self.with_view(|view| view.modes.mouse_reporting) else {
            return;
        };

        if mouse_reporting && self.controlling {
            let Some(local) = cursor.position_in(bounds) else {
                return;
            };
            let grid = self.grid(bounds.size());
            let (col, row) = cell_at(local, metrics, grid);
            let button = if notches > 0.0 {
                zeughaus_mux::MouseButton::WheelUp
            } else {
                zeughaus_mux::MouseButton::WheelDown
            };
            // One report per notch, bounded: a kinetic wheel can deliver a
            // very large delta in one event.
            let count = (notches.abs().round() as u32).clamp(1, 16);
            for _ in 0..count {
                let serial = state.next_serial();
                self.publish(
                    shell,
                    Action::Command(TerminalCommand::Mouse {
                        serial,
                        input: MouseInput {
                            kind: MouseKind::Press,
                            button: Some(button),
                            col,
                            row,
                            modifiers: input::modifiers(state.modifiers),
                        },
                    }),
                );
            }
            shell.capture_event();
            return;
        }

        let lines = if notches > 0.0 {
            -WHEEL_LINES
        } else {
            WHEEL_LINES
        };
        self.publish(shell, Action::ScrollBy(lines));
        shell.capture_event();
    }

    fn on_redraw(
        &self,
        state: &mut State,
        now: Instant,
        bounds: Rectangle,
        shell: &mut Shell<'_, Message>,
    ) {
        self.resize(state, now, bounds.size(), shell);

        // One lock for the two things a redraw asks the view: whether the
        // cursor blinks at all, and where it sits for the input method.
        let cursor_cell = self.with_view(|view| {
            let blinking = view.is_blinking_cursor() && view.exit.is_none();
            (blinking, view.cursor.x, view.cursor.y)
        });
        self.blink(
            state,
            now,
            cursor_cell.is_some_and(|(blinking, ..)| blinking),
            shell,
        );

        if self.focused
            && self.controlling
            && let Some((_, col, row)) = cursor_cell
        {
            let metrics = self.metrics();
            let cursor = Rectangle {
                x: bounds.x + f32::from(col) * metrics.width,
                y: bounds.y + f32::from(row) * metrics.height,
                width: metrics.width,
                height: metrics.height,
            };
            shell.request_input_method(&InputMethod::Enabled {
                cursor,
                purpose: input_method::Purpose::Terminal,
                preedit: state.preedit.as_ref().map(|content| input_method::Preedit {
                    content: content.clone(),
                    selection: None,
                    text_size: Some(self.font_size.into()),
                }),
            });
        }
    }

    /// Emits a resize once the geometry stopped moving. A drag over a split
    /// changes the pane every frame; the child only wants the answer.
    fn resize(&self, state: &mut State, now: Instant, size: Size, shell: &mut Shell<'_, Message>) {
        if !self.controlling {
            state.pending_grid = None;
            return;
        }
        let grid = self.grid(size);
        if grid == state.sent_grid {
            state.pending_grid = None;
            return;
        }
        match state.pending_grid {
            Some((pending, at)) if pending == grid => {
                if now >= at {
                    state.sent_grid = grid;
                    state.pending_grid = None;
                    self.publish(shell, Action::Command(TerminalCommand::Resize(grid)));
                } else {
                    shell.request_redraw_at(window::RedrawRequest::At(at));
                }
            }
            _ => {
                let at = now + RESIZE_DEBOUNCE;
                state.pending_grid = Some((grid, at));
                shell.request_redraw_at(window::RedrawRequest::At(at));
            }
        }
    }

    /// The only animation in the widget, and only while this pane has the
    /// keyboard: an unfocused or non-blinking terminal asks for no frames.
    /// `blinking` is what the view said, read before the lock was released.
    fn blink(
        &self,
        state: &mut State,
        now: Instant,
        blinking: bool,
        shell: &mut Shell<'_, Message>,
    ) {
        if !self.focused || !blinking {
            state.blink_on = true;
            state.blink_at = None;
            return;
        }
        match state.blink_at {
            Some(at) if now >= at => {
                state.blink_on = !state.blink_on;
                let next = now + BLINK_INTERVAL;
                state.blink_at = Some(next);
                shell.request_redraw();
                shell.request_redraw_at(window::RedrawRequest::At(next));
            }
            Some(at) => shell.request_redraw_at(window::RedrawRequest::At(at)),
            None => {
                let next = now + BLINK_INTERVAL;
                state.blink_at = Some(next);
                shell.request_redraw_at(window::RedrawRequest::At(next));
            }
        }
    }

    // ----------------------------------------------------------- render ---

    fn frame(&self, view: &TerminalView, state: &State, size: Size, style: &Style) -> Frame {
        let metrics = self.metrics();
        let grid = grid_size(size, metrics);
        let top = top_row(view);

        let cursor_row = i64::from(view.cursor.y) + view.visible.start;
        let preedit = state
            .preedit
            .as_deref()
            .filter(|preedit| !preedit.is_empty() && self.focused);

        let mut lines = Vec::with_capacity(usize::from(grid.rows));
        for index in 0..grid.rows {
            let stable = top + i64::from(index);
            let mut spans = view
                .row(stable)
                .map(|row| row.spans.clone())
                .unwrap_or_default();
            if let Some(preedit) = preedit.filter(|_| stable == cursor_row) {
                overlay_preedit(&mut spans, view.cursor.x, preedit, grid.cols);
            }
            lines.push(FrameRow {
                key: row_key(&spans, self.font_size),
                spans,
            });
        }

        let selection = state
            .selection
            .as_ref()
            .map(|selection| selection.resolve(view))
            .map(|range| {
                (0..grid.rows)
                    .filter_map(|index| {
                        let stable = top + i64::from(index);
                        selection::row_span(range, stable, grid.cols).map(|(from, to)| Highlight {
                            row: index,
                            from,
                            to,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let cursor = self.cursor_spec(view, state, grid, top, cursor_row);

        // What the child set stands; the theme fills the rest. The generation
        // covers the merged palette, so switching themes rebuilds instances
        // without touching a single shaped row.
        let palette = view.palette.themed(&style.palette);

        Frame {
            cols: grid.cols,
            rows: grid.rows,
            metrics,
            font_size: self.font_size,
            palette_generation: palette_generation(&palette),
            palette,
            reverse_video: view.modes.reverse_video,
            lines,
            selection,
            selection_color: style.selection,
            cursor,
        }
    }

    /// The frame of a pane whose view has not arrived yet: the right size,
    /// the theme's palette, and nothing on it.
    pub(crate) fn empty_frame(&self, size: Size, style: &Style) -> Frame {
        let metrics = self.metrics();
        let grid = grid_size(size, metrics);
        let palette = style.palette.clone();
        Frame {
            cols: grid.cols,
            rows: grid.rows,
            metrics,
            font_size: self.font_size,
            palette_generation: palette_generation(&palette),
            palette,
            reverse_video: false,
            lines: Vec::new(),
            selection: Vec::new(),
            selection_color: style.selection,
            cursor: None,
        }
    }

    fn cursor_spec(
        &self,
        view: &TerminalView,
        state: &State,
        grid: Dimensions,
        top: i64,
        cursor_row: i64,
    ) -> Option<CursorSpec> {
        if !view.cursor.visible || view.exit.is_some() || !state.blink_on {
            return None;
        }
        // Scrolled into history, the cursor is not on screen at all.
        let row = u16::try_from(cursor_row - top).ok()?;
        if row >= grid.rows || view.cursor.x >= grid.cols {
            return None;
        }
        Some(CursorSpec {
            col: view.cursor.x,
            row,
            shape: view.cursor.shape,
            focused: self.focused,
        })
    }
}

/// The stable row the top of the pane shows.
fn top_row(view: &TerminalView) -> i64 {
    view.viewport().start
}

/// Draws the IME's pre-edit where the cursor is, underlined, so composing
/// happens on the spot instead of in a floating box.
fn overlay_preedit(spans: &mut Vec<CellSpan>, col: u16, preedit: &str, cols: u16) {
    let cell_count = preedit.chars().count().min(usize::from(cols)) as u16;
    if cell_count == 0 {
        return;
    }
    let end = col.saturating_add(cell_count);
    spans.retain(|span| {
        span.start_col.saturating_add(span.cell_count) <= col || span.start_col >= end
    });
    spans.push(CellSpan {
        start_col: col,
        cell_count,
        text: preedit.to_string(),
        style: CellStyle {
            flags: StyleFlags::default().with_underline(Underline::Single),
            ..CellStyle::default()
        },
        link: None,
    });
    spans.sort_by_key(|span| span.start_col);
}

impl<'a, Message, Theme, Renderer> From<Terminal<'a, Message, Theme>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: Catalog + 'a,
    Renderer: iced_wgpu::primitive::Renderer + 'a,
{
    fn from(terminal: Terminal<'a, Message, Theme>) -> Self {
        Element::new(terminal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start_col: u16, text: &str) -> CellSpan {
        CellSpan {
            start_col,
            cell_count: text.chars().count() as u16,
            text: text.to_string(),
            style: CellStyle::default(),
            link: None,
        }
    }

    #[test]
    fn a_preedit_replaces_the_cells_it_covers() {
        let mut spans = vec![span(0, "abc"), span(4, "defg")];
        overlay_preedit(&mut spans, 4, "xy", 20);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "abc");
        assert_eq!(spans[1].text, "xy");
        assert_eq!(spans[1].start_col, 4);
        assert!(spans[1].style.flags.underline() != Underline::None);
    }

    #[test]
    fn a_preedit_wider_than_the_grid_is_clamped() {
        let mut spans = Vec::new();
        overlay_preedit(&mut spans, 0, "abcdef", 3);
        assert_eq!(spans[0].cell_count, 3);
    }
}
