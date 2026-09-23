use iced::Element;
use iced_palette::{Command, command, command_palette};
use zeughaus_core::NodeDefinition;
use zeughaus_mux::DetachedTerminal;
use zeughaus_theme::Theme;

use crate::message::Message;

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

/// What the palette offers about the runner this editor is attached to.
///
/// Both halves are empty without one: a detached terminal is the runner's own
/// -- a job's, until somebody looks at it -- and holding needs an address to
/// ask. The browser editor has no sync layer and therefore never has either.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunnerState<'a> {
    /// Terminals the runner owns that no pane shows, from the last workspace
    /// snapshot.
    pub detached: &'a [DetachedTerminal],
    /// Whether an endpoint to ask is known.
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
        command("session.copy", "Session / Copy Session ID".to_string())
            .description("Copy the current collaboration session id to the clipboard")
            .action(Message::CopySessionId),
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

/// One attach and one close entry per terminal the runner owns and no pane
/// shows, plus the hold switch while a runner can be reached.
fn runner_commands(runner: &RunnerState<'_>) -> Vec<Command<Message>> {
    let mut commands = Vec::with_capacity(runner.detached.len() * 2 + 2);
    for detached in runner.detached {
        // A terminal the runner never titled is still worth listing, and its
        // id is the only name it has.
        let title = if detached.title.is_empty() {
            format!("terminal {}", detached.terminal.0)
        } else {
            detached.title.clone()
        };
        commands.push(
            command(ATTACH_COMMAND_ID, format!("Terminal / Attach: {title}"))
                .description("Show this terminal of the runner's in a new tab")
                .action(Message::AttachTerminal(detached.terminal)),
        );
        commands.push(
            command(CLOSE_COMMAND_ID, format!("Terminal / Close: {title}"))
                .description("Kill this terminal of the runner's and forget it")
                .action(Message::CloseTerminal(detached.terminal)),
        );
    }
    if runner.reachable {
        commands.push(
            command("runner.hold", "Runner / Hold".to_string())
                .description("Start no new runs and let the live ones finish")
                .action(Message::HoldRunner(true)),
        );
        commands.push(
            command("runner.release", "Runner / Release".to_string())
                .description("Start runs again")
                .action(Message::HoldRunner(false)),
        );
    }
    commands
}

/// Render the command palette overlay.
pub fn view<'a>(
    input: &str,
    commands: &[Command<Message>],
    selected_index: usize,
) -> Element<'a, Message, iced::Theme> {
    command_palette(
        input,
        commands,
        selected_index,
        Message::PaletteInput,
        Message::PaletteSelect,
        Message::PaletteNavigate,
        || Message::PaletteCancel,
    )
}
