mod app;
mod message;

use app::App;

fn main() -> iced::Result {
    iced::application(App::new, App::update, App::view)
        .title("Zeughaus Editor")
        .theme(|app: &App| app.theme())
        .window(iced::window::Settings {
            size: iced::Size::new(1280.0, 800.0),
            position: iced::window::Position::Centered,
            ..Default::default()
        })
        .run()
}
