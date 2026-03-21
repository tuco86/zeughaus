use iced::Element;
use iced::Theme;
use iced_palette::{Command, command, command_palette};
use zeughaus_core::NodeDefinition;

use crate::message::Message;

/// Build the list of palette commands from the node catalog.
pub fn build_commands(catalog: &[NodeDefinition]) -> Vec<Command<Message>> {
    catalog
        .iter()
        .map(|def| {
            command(def.type_id, def.display_name)
                .description(format!("[{}]", def.category))
                .action(Message::SpawnNode {
                    type_id: def.type_id.to_string(),
                })
        })
        .collect()
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
