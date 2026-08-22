use iced::Element;
use iced::Theme;
use iced_palette::{Command, command, command_palette};
use zeughaus_core::NodeDefinition;

use crate::message::Message;

/// `iced_palette::CommandId` is a `&'static str`, but a node's `type_id` is
/// runtime data (`Arc<str>`) since the type-system rewrite. The palette used
/// here filters on name plus description and reports the selection by index,
/// and the spawn target travels in the message, so every catalog entry shares
/// this id and carries its type id in the description.
const SPAWN_COMMAND_ID: &str = "node.spawn";

/// Build the list of palette commands from the node catalog, sorted by category.
pub fn build_commands(catalog: &[NodeDefinition]) -> Vec<Command<Message>> {
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
    ];

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

/// Render the command palette overlay.
pub fn view<'a>(
    input: &str,
    commands: &[Command<Message>],
    selected_index: usize,
) -> Element<'a, Message, Theme> {
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
