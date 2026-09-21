mod app;
// The video path: dialling the runtime's frame feed. Native-only, because it
// needs the sync layer to learn where a runtime serves.
#[cfg(not(target_arch = "wasm32"))]
mod feed;
mod message;
// The terminal path: the runner's mux, its shared workspace and one stream
// per terminal. Native-only, for the same reason as `feed`.
#[cfg(not(target_arch = "wasm32"))]
mod mux;
mod palette;
// Settings edits waiting to reach the store. Native-only: without a store
// there is nothing to hold them back from.
#[cfg(not(target_arch = "wasm32"))]
mod pending;
// The weida client under the feed: the runtime, the trust, the first dial.
#[cfg(not(target_arch = "wasm32"))]
mod transport;
mod workspace;

use app::App;

/// The window the editor opens with. Also what [`App`] assumes it is showing
/// until the first resize, so a node the palette places lands in the middle
/// of it either way.
pub(crate) const WINDOW_SIZE: iced::Size = iced::Size::new(1280.0, 800.0);

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

    // `zeughaus`            -> edits a local scratch graph unless a store is
    //                          reachable, then that store is the document
    // `zeughaus join <id>`  -> join collaboration session <id>
    #[cfg(not(target_arch = "wasm32"))]
    let session = parse_join_arg();
    #[cfg(target_arch = "wasm32")]
    let session: Option<String> = None;

    let app = iced::application(move || App::new(session.clone()), App::update, App::view)
        .subscription(App::subscription)
        .title("Zeughaus Editor")
        .theme(|app: &App| app.theme());
    // The terminal's font is bundled, not looked up: a terminal grid needs
    // every glyph at one advance width, and whatever the host has installed
    // does not promise that. The wasm editor draws no terminal.
    #[cfg(not(target_arch = "wasm32"))]
    let app = iced_terminal::font_bytes().fold(app, |app, bytes| app.font(bytes));
    app.window(iced::window::Settings {
        size: WINDOW_SIZE,
        position: iced::window::Position::Centered,
        // The close is handled rather than obeyed: a settings edit is held
        // back for 400 ms after the last keystroke, and closing the window
        // in that window has to flush it to the store rather than drop it.
        // `App` flushes and then ends the runtime itself. Set here rather
        // than through `exit_on_close_request`, which `window` would
        // overwrite.
        exit_on_close_request: false,
        ..Default::default()
    })
    .run()
}

/// Parses `join <sessionid>` from the CLI args. Returns the session token to
/// join, or `None` to host the default session on this machine's store.
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
