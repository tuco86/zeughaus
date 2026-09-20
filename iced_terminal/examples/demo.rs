//! A terminal surface with nothing behind it.
//!
//! Builds a [`TerminalView`] from a hand-made [`TerminalHead`] -- no runner,
//! no PTY, no QUIC -- so the widget, the font and the pipeline can be looked
//! at on their own. Typing reports actions to the status line instead of
//! sending them anywhere.
//!
//! `cargo run -p iced_terminal --example demo`

use iced::widget::{column, container, text};
use iced::{Element, Fill, Task};

use iced_terminal::{Action, Terminal};
use zeughaus_mux::view::TerminalView;
use zeughaus_mux::{
    CellSpan, CellStyle, Cursor, CursorShape, Dimensions, Modes, Palette, RowData, StableRange,
    StyleFlags, TerminalHead, TerminalId, Underline, WireColor,
};

const COLS: u16 = 64;
const ROWS: u16 = 12;

fn main() -> iced::Result {
    let mut application = iced::application(Demo::new, Demo::update, Demo::view)
        .title("iced_terminal demo")
        .theme(theme);
    for font in iced_terminal::font_bytes() {
        application = application.font(font);
    }
    application.run()
}

fn theme(_state: &Demo) -> iced::Theme {
    iced::Theme::Dark
}

struct Demo {
    view: TerminalView,
    last: String,
    serial: u64,
}

#[derive(Debug, Clone)]
enum Message {
    Terminal(Action),
}

impl Demo {
    fn new() -> (Self, Task<Message>) {
        (
            Demo {
                view: TerminalView::from_head(head(), 1024),
                last: "press a key, drag a selection, scroll".to_string(),
                serial: 1,
            },
            Task::none(),
        )
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let Message::Terminal(action) = message;
        match action {
            Action::ScrollBy(lines) => {
                let _ = self.view.scroll_by(lines);
                self.last = format!("scroll by {lines}");
            }
            Action::Command(command) => {
                if let Some(serial) = command.serial() {
                    self.serial = serial + 1;
                }
                self.last = format!("{command:?}");
            }
            other => self.last = format!("{other:?}"),
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let terminal = Terminal::new(&self.view, 1)
            .controlling(true)
            .focused(true)
            .next_serial(self.serial)
            .font_size(16.0)
            .on_action(Message::Terminal);

        column![
            container(terminal).width(Fill).height(Fill),
            text(&self.last).size(12),
        ]
        .padding(8)
        .spacing(8)
        .into()
    }
}

fn span(start_col: u16, content: &str, style: CellStyle) -> CellSpan {
    CellSpan {
        start_col,
        cell_count: content.chars().count() as u16,
        text: content.to_string(),
        style,
        link: None,
    }
}

fn styled(fg: WireColor, flags: u16) -> CellStyle {
    CellStyle {
        fg,
        flags: StyleFlags(flags),
        ..CellStyle::default()
    }
}

fn head() -> TerminalHead {
    let plain = CellStyle::default();
    let mut rows = Vec::new();
    let mut push = |index: i64, wrapped: bool, spans: Vec<CellSpan>| {
        rows.push(RowData {
            stable_row: index,
            row_seq: 1,
            wrapped,
            spans,
        });
    };

    push(
        0,
        false,
        vec![span(
            0,
            "iced_terminal demo",
            styled(WireColor::Indexed(10), StyleFlags::BOLD),
        )],
    );
    push(1, false, Vec::new());
    push(
        2,
        false,
        vec![
            span(0, "bold ", styled(WireColor::Default, StyleFlags::BOLD)),
            span(5, "dim ", styled(WireColor::Default, StyleFlags::DIM)),
            span(
                9,
                "reverse ",
                styled(WireColor::Default, StyleFlags::REVERSE),
            ),
            span(
                17,
                "strike",
                styled(WireColor::Default, StyleFlags::STRIKETHROUGH),
            ),
        ],
    );
    push(
        3,
        false,
        vec![
            span(
                0,
                "underline",
                CellStyle {
                    flags: StyleFlags::default().with_underline(Underline::Single),
                    ..plain
                },
            ),
            span(
                10,
                "curly",
                CellStyle {
                    fg: WireColor::Indexed(9),
                    flags: StyleFlags::default().with_underline(Underline::Curly),
                    ..plain
                },
            ),
            span(
                16,
                "dashed",
                CellStyle {
                    flags: StyleFlags::default().with_underline(Underline::Dashed),
                    ..plain
                },
            ),
        ],
    );
    push(
        4,
        false,
        vec![
            span(0, "256 colours:", plain),
            span(13, "\u{2588}", styled(WireColor::Indexed(196), 0)),
            span(14, "\u{2588}", styled(WireColor::Indexed(202), 0)),
            span(15, "\u{2588}", styled(WireColor::Indexed(226), 0)),
            span(16, "\u{2588}", styled(WireColor::Indexed(46), 0)),
            span(17, "\u{2588}", styled(WireColor::Indexed(51), 0)),
            span(18, "\u{2588}", styled(WireColor::Indexed(21), 0)),
            span(19, "\u{2588}", styled(WireColor::Indexed(201), 0)),
        ],
    );
    push(
        5,
        false,
        vec![span(
            0,
            "unicode: \u{4e16}\u{754c} e\u{0301} \u{1f980} \u{e0b0}",
            plain,
        )],
    );
    push(
        6,
        true,
        vec![span(0, "a wrapped logical line that ", plain)],
    );
    push(7, false, vec![span(0, "continues on the next row", plain)]);
    push(
        8,
        false,
        vec![span(0, "$ ", styled(WireColor::Indexed(12), 0))],
    );

    TerminalHead {
        terminal: TerminalId(1),
        epoch: 1,
        seq: 1,
        dimensions: Dimensions {
            cols: COLS,
            rows: ROWS,
        },
        visible: StableRange {
            start: 0,
            end: i64::from(ROWS),
        },
        first_retained: 0,
        cursor: Cursor {
            x: 2,
            y: 8,
            shape: CursorShape::Block,
            visible: true,
            blinking: true,
        },
        title: "demo".to_string(),
        modes: Modes::default(),
        palette: Palette::default(),
        rows,
        exit: None,
        controller: None,
        input_serial_ack: 0,
    }
}
