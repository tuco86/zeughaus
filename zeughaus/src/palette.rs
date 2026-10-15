use iced::Element;
use iced_palette::{Command, command, filter_commands, fuzzy_match};
use zeughaus_core::NodeDefinition;
use zeughaus_mux::{Axis, DetachedTerminal};
use zeughaus_theme::Theme;

use crate::message::Message;
use crate::workspace::{self, RunnerKey};

/// `iced_palette::CommandId` is a `&'static str`, but a node's `type_id` is
/// runtime data (`Arc<str>`) since the type-system rewrite. The palette used
/// here filters on name plus description and reports the selection by index,
/// and the spawn target travels in the message, so every catalog entry shares
/// this id and carries its type id in the description.
const SPAWN_COMMAND_ID: &str = "node.spawn";

/// The same reason as [`SPAWN_COMMAND_ID`]: which terminal is runtime data,
/// so every entry shares one id and carries the terminal in its message.
const ATTACH_COMMAND_ID: &str = "terminal.attach";
const CLOSE_COMMAND_ID: &str = "terminal.close";
/// Same again for a theme: which one is a name, the pack and the state
/// directory decide how many there are, and the window resolves the name
/// against the list it built at startup.
const THEME_COMMAND_ID: &str = "editor.theme";
/// Which runner is runtime data too.
const HOLD_COMMAND_ID: &str = "runner.hold";
const RELEASE_COMMAND_ID: &str = "runner.release";

/// What the palette offers about the runners this editor is connected to.
///
/// All of it is empty without one: a detached terminal is a runner's own
/// -- a job's, until somebody looks at it -- holding needs an address to
/// ask, and the pane commands change a runner's workspace. The browser
/// editor has no sync layer and therefore never has any of it.
#[derive(Debug, Clone, Default)]
pub struct RunnerState<'a> {
    /// One entry per connected runner, in section order.
    pub runners: Vec<RunnerPalette<'a>>,
    /// Whether the focused pane's runner takes structural commands.
    pub pane_commands: bool,
}

/// What one runner offers.
#[derive(Debug, Clone)]
pub struct RunnerPalette<'a> {
    pub key: RunnerKey,
    /// The section label, which the commands name the runner by.
    pub label: &'a str,
    /// Terminals the runner owns that no pane shows, from its last
    /// workspace snapshot.
    pub detached: &'a [DetachedTerminal],
    /// Whether its endpoint is known, so it can be held.
    pub reachable: bool,
}

/// Build the list of palette commands: the editor's own, what the runner
/// currently offers, then the node catalog sorted by category.
pub fn build_commands(
    catalog: &[NodeDefinition],
    runner: &RunnerState<'_>,
    themes: &[Theme],
) -> Vec<Command<Message>> {
    let mut sorted: Vec<&NodeDefinition> = catalog.iter().collect();
    sorted.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then(a.display_name.cmp(&b.display_name))
    });

    // Non-node commands first.
    let mut commands = vec![
        command("graph.autolayout", "Graph / Auto Layout".to_string())
            .description("Arrange the nodes of the current graph in columns by depth")
            .action(Message::AutoLayout),
    ];

    commands.extend(runner_commands(runner));
    commands.extend(themes.iter().map(|theme| {
        command(THEME_COMMAND_ID, format!("Theme / {}", theme.name()))
            .description("Draw the editor, the graph and the terminals with this scheme")
            .action(Message::SetTheme(theme.name().to_owned()))
    }));

    commands.extend(sorted.iter().map(|def| {
        command(
            SPAWN_COMMAND_ID,
            format!("{} / {}", def.category, def.display_name),
        )
        .description(def.type_id.to_string())
        .action(Message::SpawnNode {
            type_id: def.type_id.to_string(),
        })
    }));

    commands
}

/// The pane commands while the focused pane's runner is attached, one attach
/// and one close entry per terminal a runner owns and no pane shows, plus the
/// hold switch per runner that can be reached.
fn runner_commands(state: &RunnerState<'_>) -> Vec<Command<Message>> {
    let mut commands = Vec::with_capacity(3 + state.runners.len() * 2);
    if state.pane_commands {
        commands.push(
            command(
                "pane.split.horizontal",
                "Pane / Split Horizontal".to_string(),
            )
            .description("Put a new terminal beside the focused pane")
            .action(Message::Workspace(workspace::Message::SplitFocused(
                Axis::Horizontal,
            ))),
        );
        commands.push(
            command("pane.split.vertical", "Pane / Split Vertical".to_string())
                .description("Put a new terminal below the focused pane")
                .action(Message::Workspace(workspace::Message::SplitFocused(
                    Axis::Vertical,
                ))),
        );
        commands.push(
            command("pane.close", "Pane / Close".to_string())
                .description("Close the focused pane; a job's terminal is only detached")
                .action(Message::Workspace(workspace::Message::CloseFocused)),
        );
    }
    for runner in &state.runners {
        for detached in runner.detached {
            // A terminal the runner never titled is still worth listing, and
            // its id is the only name it has.
            let title = if detached.title.is_empty() {
                format!("terminal {}", detached.terminal.0)
            } else {
                detached.title.clone()
            };
            commands.push(
                command(
                    ATTACH_COMMAND_ID,
                    format!("Terminal / Attach {} / {title}", runner.label),
                )
                .description("Open this terminal of the runner in a new tab")
                .action(Message::AttachTerminal(
                    runner.key.clone(),
                    detached.terminal,
                )),
            );
            commands.push(
                command(
                    CLOSE_COMMAND_ID,
                    format!("Terminal / Close {} / {title}", runner.label),
                )
                .description("Kill this terminal on the runner and forget it")
                .action(Message::CloseTerminal(
                    runner.key.clone(),
                    detached.terminal,
                )),
            );
        }
        if runner.reachable {
            commands.push(
                command(HOLD_COMMAND_ID, format!("Runner / Hold {}", runner.label))
                    .description("Start no new runs and let the live ones finish")
                    .action(Message::HoldRunner(runner.key.clone(), true)),
            );
            commands.push(
                command(
                    RELEASE_COMMAND_ID,
                    format!("Runner / Release {}", runner.label),
                )
                .description("Start runs again")
                .action(Message::HoldRunner(runner.key.clone(), false)),
            );
        }
    }
    commands
}

/// Width and list height of the panel, and the fixed height of one row: a
/// title over a description, so a row with no description is as tall as one
/// with it and the list does not jump while the query narrows it.
const PANEL_WIDTH: f32 = 640.0;
const LIST_HEIGHT: f32 = 440.0;
const ROW_HEIGHT: f32 = 46.0;
/// Distance from the top of the window to the panel.
const PANEL_TOP: f32 = 80.0;
const SP_2: f32 = 8.0;
const SP_3: f32 = 12.0;
const TEXT_MD: f32 = 14.0;
const TEXT_SM: f32 = 12.0;
/// How many characters of a title or a description a row shows. Text is
/// never wrapped, so what does not fit is shortened in the middle: the end
/// of a terminal's title (its directory) says as much as its start.
const TITLE_CHARS: usize = 68;
const DETAIL_CHARS: usize = 84;

/// What a row shows of `text`: at most `max` characters, the middle replaced
/// by an ellipsis, each paired with its index in `text` (`None` for the
/// ellipsis) so match highlights survive the shortening.
fn shorten(text: &str, max: usize) -> Vec<(char, Option<usize>)> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return chars
            .into_iter()
            .enumerate()
            .map(|(i, c)| (c, Some(i)))
            .collect();
    }
    let tail = max / 3;
    let head = max - tail - 1;
    let mut out: Vec<(char, Option<usize>)> = chars[..head]
        .iter()
        .enumerate()
        .map(|(i, &c)| (c, Some(i)))
        .collect();
    out.push(('\u{2026}', None));
    let start = chars.len() - tail;
    out.extend(
        chars[start..]
            .iter()
            .enumerate()
            .map(|(i, &c)| (c, Some(start + i))),
    );
    out
}

/// One unwrapped line of text with the characters the query matched drawn
/// apart: in the accent colour, or underlined on the selected row, where the
/// accent is the background.
fn line<'a>(
    text: &str,
    query: &str,
    max: usize,
    size: f32,
    color: Option<iced::Color>,
    selected: bool,
    theme: &Theme,
) -> Element<'a, Message, Theme> {
    use iced::widget::text::{Rich, Span};

    let matched = fuzzy_match(query, text)
        .map(|m| m.indices)
        .unwrap_or_default();
    let mut spans: Vec<Span<'a, (), iced::Font>> = Vec::new();
    let mut run = String::new();
    let mut run_matched = false;
    let mut flush = |run: &mut String, matched: bool| {
        if run.is_empty() {
            return;
        }
        let mut span = Span::new(std::mem::take(run));
        if let Some(color) = color {
            span = span.color(color);
        }
        if matched {
            span = if selected {
                span.underline(true)
            } else {
                span.color(theme.accent())
            };
        }
        spans.push(span);
    };
    for (c, original) in shorten(text, max) {
        let hit = original.is_some_and(|i| matched.contains(&i));
        if hit != run_matched {
            flush(&mut run, run_matched);
            run_matched = hit;
        }
        run.push(c);
    }
    flush(&mut run, run_matched);
    Rich::with_spans(spans)
        .size(size)
        .wrapping(iced::widget::text::Wrapping::None)
        .into()
}

/// The close control: the codicon close glyph.
fn close_button<'a>() -> Element<'a, Message, Theme> {
    let label = iced::widget::text("\u{ea76}").size(TEXT_SM);
    iced::widget::button(label)
        .padding([4.0, SP_2])
        .on_press(Message::PaletteCancel)
        .style(|theme: &Theme, status| {
            let hovered = matches!(
                status,
                iced::widget::button::Status::Hovered | iced::widget::button::Status::Pressed
            );
            iced::widget::button::Style {
                background: hovered.then(|| theme.extended().background.strong.color.into()),
                text_color: if hovered {
                    theme.extended().background.strong.text
                } else {
                    theme.muted()
                },
                border: iced::border::rounded(4.0),
                ..Default::default()
            }
        })
        .into()
}

/// The palette's list, which [`reveal`] scrolls.
const LIST_ID: &str = "palette-list";

/// Scrolls the palette's list so that entry `at` of `count` is in view: the
/// list moves in step with the selection, which keeps the selected row near
/// where the eye already is.
pub fn reveal<T: Send + 'static>(at: usize, count: usize) -> iced::Task<T> {
    let y = if count > 1 {
        at as f32 / (count - 1) as f32
    } else {
        0.0
    };
    iced::widget::operation::snap_to(
        LIST_ID,
        iced::widget::scrollable::RelativeOffset {
            x: None,
            y: Some(y),
        },
    )
}

/// Render the command palette: a full-window scrim, which a click dismisses,
/// over one panel at the top.
pub fn view<'a>(
    input: &str,
    theme: &Theme,
    commands: &[Command<Message>],
    selected_index: usize,
) -> Element<'a, Message, Theme> {
    use iced::widget::{
        Column, button, column, container, mouse_area, opaque, row, scrollable, text, text_input,
    };
    use iced::{Alignment, Border, Length, Padding, Shadow, Vector};

    let filtered = filter_commands(input, commands);
    let mut rows = Column::new().spacing(2.0).width(Length::Fill);
    for (display_index, (original, _)) in filtered.iter().enumerate() {
        let command = &commands[*original];
        let selected = display_index == selected_index;
        let extended = theme.extended();
        let detail_color = if selected {
            extended.primary.base.text.scale_alpha(0.75)
        } else {
            theme.muted()
        };

        let mut label = Column::new().width(Length::Fill).push(line(
            &command.name,
            input,
            TITLE_CHARS,
            TEXT_MD,
            None,
            selected,
            theme,
        ));
        if let Some(description) = &command.description {
            // Not highlighted: a fuzzy query scatters single letters over a
            // sentence, which reads as noise.
            label = label.push(line(
                description,
                "",
                DETAIL_CHARS,
                TEXT_SM,
                Some(detail_color),
                selected,
                theme,
            ));
        }
        let mut content = row![container(label).width(Length::Fill).clip(true)]
            .spacing(SP_3)
            .align_y(Alignment::Center);
        if let Some(shortcut) = &command.shortcut {
            content = content.push(text(shortcut.display()).size(TEXT_SM).color(detail_color));
        }

        let entry = button(content)
            .on_press(Message::PaletteSelect(display_index))
            .padding([0.0, SP_3])
            .width(Length::Fill)
            .height(ROW_HEIGHT)
            .style(move |theme: &Theme, _status| {
                let extended = theme.extended();
                if selected {
                    button::Style {
                        background: Some(extended.primary.base.color.into()),
                        text_color: extended.primary.base.text,
                        border: iced::border::rounded(4.0),
                        ..Default::default()
                    }
                } else {
                    button::Style {
                        text_color: extended.background.base.text,
                        border: iced::border::rounded(4.0),
                        ..Default::default()
                    }
                }
            });
        rows = rows.push(mouse_area(entry).on_enter(Message::PaletteNavigate(display_index)));
    }
    if filtered.is_empty() {
        rows = rows.push(
            container(
                text("No matching command")
                    .size(TEXT_SM)
                    .color(theme.muted()),
            )
            .padding(SP_3),
        );
    }

    let search = text_input("Type to search...", input)
        .id(iced_palette::INPUT_ID)
        .on_input(Message::PaletteInput)
        .padding([SP_2, SP_3])
        .size(TEXT_MD)
        .width(Length::Fill);
    let header = row![search, close_button()]
        .spacing(SP_2)
        .align_y(Alignment::Center);
    let list = container(scrollable(rows).id(LIST_ID).spacing(SP_2)).max_height(LIST_HEIGHT);
    let panel = container(column![header, list].spacing(SP_2).padding(SP_2))
        .max_width(PANEL_WIDTH)
        .style(|theme: &Theme| container::Style {
            background: Some(theme.chrome().into()),
            border: Border {
                color: theme.extended().background.strong.color,
                width: 1.0,
                radius: 6.0.into(),
            },
            shadow: Shadow {
                color: iced::Color::BLACK.scale_alpha(0.35),
                offset: Vector::new(0.0, 4.0),
                blur_radius: 16.0,
            },
            ..Default::default()
        });
    // `opaque` keeps a click on the panel's own padding from reaching the
    // scrim and closing the palette.
    mouse_area(
        container(opaque(panel))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .padding(Padding::ZERO.top(PANEL_TOP))
            .style(|theme: &Theme| container::Style {
                background: Some(
                    theme
                        .extended()
                        .background
                        .base
                        .color
                        .scale_alpha(0.6)
                        .into(),
                ),
                ..Default::default()
            }),
    )
    .on_press(Message::PaletteCancel)
    .into()
}

#[cfg(test)]
mod tests {
    use super::shorten;

    fn shown(text: &str, max: usize) -> String {
        shorten(text, max).into_iter().map(|(c, _)| c).collect()
    }

    #[test]
    fn short_text_is_untouched() {
        assert_eq!(shown("Graph / Auto Layout", 40), "Graph / Auto Layout");
    }

    #[test]
    fn long_text_keeps_both_ends_within_the_limit() {
        let title =
            "Terminal / Attach 127.0.0.1 6fe884d4 / hannes@tuco-pc:/tmp/zh-agent/ci/work/gate";
        let cut = shorten(title, 40);

        assert_eq!(cut.len(), 40);
        let text: String = cut.iter().map(|(c, _)| c).collect();
        assert!(text.starts_with("Terminal / Attach"));
        assert!(text.ends_with("work/gate"));
        assert_eq!(cut.iter().filter(|(_, i)| i.is_none()).count(), 1);
    }
}
