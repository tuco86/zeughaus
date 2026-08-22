mod app;
mod message;
mod palette;
// The SpacetimeDB client lives in its own crate, shared with the headless
// runtime process. Re-exported under the old paths so `crate::sync::` and
// `crate::module_bindings::` keep working. Native-only for now; the wasm editor
// sync path is a later step.
#[cfg(not(target_arch = "wasm32"))]
pub use zeughaus_sync as sync;
#[cfg(not(target_arch = "wasm32"))]
pub use zeughaus_sync::module_bindings;

use app::App;

fn main() -> iced::Result {
    // Surface Rust panics in the browser console on wasm.
    #[cfg(target_arch = "wasm32")]
    console_error_panic_hook::set_once();

    // Give this process a unique id range so two collaborating editors sharing
    // a SpacetimeDB store never assign colliding node/edge ids.
    #[cfg(not(target_arch = "wasm32"))]
    {
        zeughaus_core::NodeId::seed_unique();
        zeughaus_core::EdgeId::seed_unique();
    }

    // `zeughaus`            -> local editor (no sync)
    // `zeughaus join <id>`  -> join collaboration session <id>
    #[cfg(not(target_arch = "wasm32"))]
    let session = parse_join_arg();
    #[cfg(target_arch = "wasm32")]
    let session: Option<String> = None;

    iced::application(move || App::new(session.clone()), App::update, App::view)
        .subscription(App::subscription)
        .title("Zeughaus Editor")
        .theme(|app: &App| app.theme())
        .window(iced::window::Settings {
            size: iced::Size::new(1280.0, 800.0),
            position: iced::window::Position::Centered,
            ..Default::default()
        })
        .run()
}

/// Parses `join <sessionid>` from the CLI args. Returns the session id to join,
/// or `None` for a local (unsynced) editor.
#[cfg(not(target_arch = "wasm32"))]
fn parse_join_arg() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "join" {
            return args.next();
        }
    }
    None
}
