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
// What this window looks like and where it keeps that. Native-only: the
// browser editor has no state directory.
#[cfg(not(target_arch = "wasm32"))]
mod prefs;
// The headless editor and its control client, behind the `remote` feature.
#[cfg(all(feature = "remote", not(target_arch = "wasm32")))]
mod remote;
// Replacing this process with a fresh build of itself on SIGUSR1.
#[cfg(unix)]
mod restart;
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
    // Before anything else: a rebuild may replace the file at any moment,
    // and only the path seen now names the binary a restart should run.
    #[cfg(unix)]
    restart::remember_exe();

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

    // `zeughaus ctl ...` and `zeughaus --headless ...` never open a window.
    #[cfg(all(feature = "remote", not(target_arch = "wasm32")))]
    if let Some(code) = remote::dispatch() {
        std::process::exit(code)
    }

    // The icon this process shows, set before the window: on macOS it belongs
    // to the application, not to a window. After `dispatch`, so that neither
    // `ctl` nor the headless host ever asks AppKit for an application object.
    #[cfg(target_os = "macos")]
    dock_icon();

    // `zeughaus`            -> edits a local scratch graph unless a store is
    //                          reachable, then that store is the document
    // `zeughaus join <id>`  -> join collaboration session <id>
    #[cfg(not(target_arch = "wasm32"))]
    let session = parse_join_arg();
    #[cfg(target_arch = "wasm32")]
    let session: Option<String> = None;

    // What the process this one replaced left for it; see `restart`.
    #[cfg(not(target_arch = "wasm32"))]
    let restore = app::restore::from_env();
    #[cfg(not(target_arch = "wasm32"))]
    let size = restore
        .as_ref()
        .map_or(WINDOW_SIZE, |r| iced::Size::new(r.width, r.height));
    #[cfg(not(target_arch = "wasm32"))]
    let maximized = restore.as_ref().is_some_and(|r| r.maximized);
    #[cfg(not(target_arch = "wasm32"))]
    let boot = move || App::boot(session.clone(), restore.clone());
    #[cfg(target_arch = "wasm32")]
    let (boot, size, maximized) = (move || App::new(session.clone()), WINDOW_SIZE, false);

    let app = iced::application(boot, App::update, App::view)
        .subscription(App::subscription)
        .title("Zeughaus Editor")
        .theme(|app: &App| app.theme())
        // Multisampling for meshes: the node header's symbols are paths, and
        // a diagonal stroke across a few pixels is jagged without it.
        .antialiasing(true);
    // The terminal's font is bundled, not looked up: a terminal grid needs
    // every glyph at one advance width, and whatever the host has installed
    // does not promise that. The wasm editor draws no terminal.
    #[cfg(not(target_arch = "wasm32"))]
    let app = iced_terminal::font_bytes().fold(app, |app, bytes| app.font(bytes));
    #[cfg(not(target_arch = "wasm32"))]
    let app = app.style(|_, theme| iced::theme::Style {
        // Only the rounded chrome corners remain transparent. The workspace
        // paints its own opaque background.
        background_color: iced::Color::TRANSPARENT,
        text_color: theme.extended().background.base.text,
    });
    app.window(iced::window::Settings {
        size,
        maximized,
        min_size: Some(iced::Size::new(640.0, 400.0)),
        position: iced::window::Position::Centered,
        // Borderless: the editor draws its own titlebar with the tab strip
        // and the window controls, and its own resize grips along the edges.
        decorations: false,
        transparent: cfg!(not(target_arch = "wasm32")),
        // The close is handled rather than obeyed: a settings edit is held
        // back for 400 ms after the last keystroke, and closing the window
        // in that window has to flush it to the store rather than drop it.
        // `App` flushes and then ends the runtime itself. Set here rather
        // than through `exit_on_close_request`, which `window` would
        // overwrite.
        exit_on_close_request: false,
        icon: window_icon(),
        platform_specific: platform_specific(),
        ..Default::default()
    })
    .run()
}

/// The application id is the window's identity to a window manager: X11's
/// `WM_CLASS`, Wayland's `app_id`. It is not what puts an icon on the
/// window -- see [`window_icon`].
#[cfg(target_os = "linux")]
fn platform_specific() -> iced::window::settings::PlatformSpecific {
    iced::window::settings::PlatformSpecific {
        application_id: APP_ID.to_owned(),
        ..Default::default()
    }
}

#[cfg(not(target_os = "linux"))]
fn platform_specific() -> iced::window::settings::PlatformSpecific {
    iced::window::settings::PlatformSpecific::default()
}

/// The application id this window reports.
#[cfg(target_os = "linux")]
pub(crate) const APP_ID: &str = "net.doodleshnookie.Zeughaus";

/// The icon a window carries: the bundled 256 px mark, decoded once at
/// startup. Every window manager that shows it smaller scales it down itself.
///
/// This reaches X11 and Windows. It reaches neither Wayland nor macOS: winit
/// 0.30 makes `set_window_icon` a no-op on both, and on macOS there is
/// nothing for it to reach -- the icon of an app there is the Dock's, which
/// [`dock_icon`] sets. On Wayland the protocol that would carry one
/// (`xdg_toplevel_icon_v1`, which KWin implements) arrived in winit 0.31;
/// until iced pins that, a compositor finds the icon through the desktop
/// entry named after [`APP_ID`], which `deploy/install.sh` installs.
#[cfg(not(target_arch = "wasm32"))]
fn window_icon() -> Option<iced::window::Icon> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(ICON_PNG))
        .read_info()
        .ok()?;
    let mut pixels = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut pixels).ok()?;
    // The generator writes 8-bit RGBA, which is what `from_rgba` wants; any
    // other encoding would have to be converted, so refuse instead.
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    pixels.truncate(info.buffer_size());
    iced::window::icon::from_rgba(pixels, info.width, info.height).ok()
}

/// The stencilled Z, compiled in: `zeughaus/assets/icon/render.py` writes it.
#[cfg(not(target_arch = "wasm32"))]
const ICON_PNG: &[u8] = include_bytes!("../assets/icon/zeughaus-256.png");

/// Gives this process the icon the Dock, the app switcher and the menu bar
/// show. A macOS app reads its icon from the `Info.plist` of the bundle it
/// runs in; this editor is a plain binary, so it hands AppKit the image
/// itself. Setting it before the window means the Dock never shows the
/// generic executable icon first.
///
/// The shared application object is created here if winit has not asked for
/// it yet; winit 0.30 takes that same one and swizzles its `sendEvent:`
/// rather than subclassing it, so asking early is safe.
#[cfg(target_os = "macos")]
fn dock_icon() {
    use objc2::AllocAnyThread;
    use objc2_app_kit::{NSApplication, NSImage};

    // Off the main thread there is no application object to talk to. `main`
    // is the main thread, so this is a check, not a fallback.
    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return;
    };
    // AppKit decodes the PNG itself: it keeps the file's resolution, which a
    // raw RGBA bitmap would have to be told about.
    let data = objc2_foundation::NSData::with_bytes(ICON_PNG);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };
    // SAFETY: `setApplicationIconImage:` is generated as unsafe only because
    // the generator cannot tell whether `None` is allowed; this passes an
    // image.
    unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
}

/// The browser tab's icon is a `<link rel="icon">` in `index.html`, not a
/// buffer the window carries.
#[cfg(target_arch = "wasm32")]
fn window_icon() -> Option<iced::window::Icon> {
    None
}

/// Parses `join <sessionid>` from the CLI args. Returns the session token to
/// join, or `None` to host the default session on this machine's store.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn parse_join_arg() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "join" {
            return args.next();
        }
    }
    None
}
