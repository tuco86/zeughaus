//! The headless host: one thread owns the [`App`], builds its interface
//! offscreen, feeds it the input the control socket asks for, and answers the
//! runtime's actions the way a window would.
//!
//! Tasks and subscriptions run on iced's tokio executor exactly as in the
//! windowed editor, and every input event is broadcast to the subscriptions,
//! so shortcuts registered through `iced::event::listen_with` fire here too.
//! Screenshots are drawn with the real cursor, so hover states show; a
//! recording additionally paints the pointer, which a video needs to be
//! followed.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use iced::advanced::clipboard::{self, Clipboard};
use iced::advanced::graphics::text::font_system;
use iced::advanced::renderer::{self, Headless};
use iced::advanced::widget::Operation;
use iced::advanced::widget::operation::{self, Outcome};
use iced::keyboard::{self, Modifiers};
use iced::{Color, Event, Point, Size, mouse};
use iced_futures::futures::StreamExt;
use iced_futures::futures::channel::mpsc as futures_mpsc;
use iced_futures::{Executor, Runtime, subscription};
use iced_runtime::core::window;
use iced_runtime::{Action, Task, UserInterface, task, user_interface};
use iced_selector::Selector;

use super::{Command, HostArgs, Keystroke};
use crate::app::App;
use crate::message::Message;

/// The frame interval while the interface asks for the next frame.
const FRAME: Duration = Duration::from_millis(16);
/// How long an input command waits for the loop to go quiet before it
/// replies. A shortcut reaches the app through a subscription, which answers
/// asynchronously; without this the next command could overtake it.
const SETTLE: Duration = Duration::from_millis(10);
/// The most an input command waits for that quiet.
const SETTLE_MAX: Duration = Duration::from_millis(250);
/// When `wait-idle` gives up.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the exit waits for connections to write their last reply.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Frame rate of `record`.
const RECORD_FPS: u32 = 30;

type HostRuntime = Runtime<iced::executor::Default, futures_mpsc::Sender<HostEvent>, HostEvent>;

/// What the executor hands back: a subscription's message or a task's action.
enum HostEvent {
    Message(Message),
    Action(Action<Message>),
}

/// Everything the loop waits on.
enum Input {
    Host(HostEvent),
    Control(Command, mpsc::Sender<String>),
}

/// Runs the headless editor until it exits, then ends the process.
pub(super) fn run(args: HostArgs) -> ! {
    // The terminal grid is measured from its bundled font; a window gets it
    // through the application builder, which this host does not use.
    for bytes in iced_terminal::font_bytes() {
        font_system()
            .write()
            .expect("font system")
            .load_font(Cow::Borrowed(bytes));
    }

    let executor = match <iced::executor::Default as Executor>::new() {
        Ok(executor) => executor,
        Err(e) => {
            eprintln!("[remote] executor: {e}");
            std::process::exit(1);
        }
    };
    // Hardware rendering only: a software fallback would hide exactly the
    // pipelines (node graph, terminal) a screenshot is taken to check.
    let renderer = Executor::block_on(
        &executor,
        <iced::Renderer as Headless>::new(iced::Font::DEFAULT, iced::Pixels(16.0), Some("wgpu")),
    );
    let Some(renderer) = renderer else {
        eprintln!("[remote] no wgpu adapter");
        std::process::exit(1);
    };

    let listener = match bind(&args.control) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("[remote] cannot listen on {}: {e}", args.control.display());
            std::process::exit(1);
        }
    };

    let (inputs_tx, inputs) = mpsc::channel();
    let (sender, mut receiver) = futures_mpsc::channel(64);
    let forward = inputs_tx.clone();
    Executor::spawn(&executor, async move {
        while let Some(event) = receiver.next().await {
            if forward.send(Input::Host(event)).is_err() {
                break;
            }
        }
    });
    let inflight = Arc::new(Inflight::default());
    {
        let inflight = inflight.clone();
        std::thread::spawn(move || accept(listener, &inputs_tx, &inflight));
    }

    let runtime = Runtime::new(executor, sender);
    let mut host = Host::new(runtime, renderer, inputs, args.size, args.scale);
    eprintln!("[remote] listening on {}", args.control.display());
    let code = host.run();
    host.close();
    inflight.drain(DRAIN_TIMEOUT);
    // A restart keeps the socket path: the next image binds it again, and a
    // client polling `size` sees it come back.
    if let Some(restore) = host.restart.take() {
        let e = crate::restart::exec(&restore);
        eprintln!("[remote] restart failed: {e}");
    }
    let _ = std::fs::remove_file(&args.control);
    // Exit rather than return: dropping the tokio runtime blocks until its
    // blocking tasks finish, and nothing is left to wait for.
    std::process::exit(code)
}

struct Host {
    app: App,
    runtime: HostRuntime,
    renderer: iced::Renderer,
    cache: user_interface::Cache,
    inputs: mpsc::Receiver<Input>,
    window: window::Id,
    /// Logical size of the pretend window.
    size: Size,
    scale: f32,
    cursor: mouse::Cursor,
    clipboard: MemoryClipboard,
    /// When the interface wants its next `RedrawRequested`.
    redraw: window::RedrawRequest,
    last_frame: Instant,
    /// When the last host event that was not a heartbeat arrived.
    last_activity: Instant,
    idle_waiters: Vec<IdleWaiter>,
    /// Commands that arrived while an input command was settling.
    queued: VecDeque<(Command, mpsc::Sender<String>)>,
    /// `quit` is answered once the app has actually ended.
    quit_replies: Vec<mpsc::Sender<String>>,
    exit: Option<i32>,
    /// Set by `Message::RestartExec`: the restore file the next image reads.
    /// The `exec` happens after the loop, once every reply is written.
    restart: Option<std::path::PathBuf>,
    /// Whether the left button is down, for the pointer a recording draws.
    pressed: bool,
    recording: Option<Recording>,
}

struct IdleWaiter {
    quiet: Duration,
    since: Instant,
    reply: mpsc::Sender<String>,
}

/// A running `record`: frames go through a bounded channel to a thread that
/// writes them to ffmpeg's stdin, so a slow encoder holds the loop back
/// instead of growing memory.
struct Recording {
    path: PathBuf,
    started: Instant,
    /// Frames sent so far; frame `n` shows the time `started + n / fps`.
    written: u64,
    frames: mpsc::SyncSender<Arc<Vec<u8>>>,
    writer: std::thread::JoinHandle<Result<(), String>>,
    child: Child,
}

impl Recording {
    /// When the next frame is due.
    fn next_frame(&self) -> Instant {
        self.started + Duration::from_secs_f64(self.written as f64 / f64::from(RECORD_FPS))
    }

    /// Closes the stream and waits for ffmpeg to write the file.
    fn finish(self) -> String {
        let Recording {
            path,
            written,
            frames,
            writer,
            mut child,
            ..
        } = self;
        // Closing the channel ends the writer, which drops ffmpeg's stdin.
        drop(frames);
        let wrote = writer
            .join()
            .unwrap_or_else(|_| Err("writer panicked".to_owned()));
        // Read before waiting: a child blocked on a full stderr pipe never
        // exits.
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        let failure = match (wrote, child.wait()) {
            (Err(e), _) => Some(e),
            (Ok(()), Err(e)) => Some(e.to_string()),
            (Ok(()), Ok(status)) if !status.success() => Some(status.to_string()),
            (Ok(()), Ok(_)) => None,
        };
        match failure {
            Some(e) => format!("err ffmpeg: {e}: {}", stderr.trim()),
            None => format!(
                "ok {} {written} frames {:.1}s",
                path.display(),
                written as f64 / f64::from(RECORD_FPS)
            ),
        }
    }
}

impl Host {
    fn new(
        runtime: HostRuntime,
        renderer: iced::Renderer,
        inputs: mpsc::Receiver<Input>,
        size: Size,
        scale: f32,
    ) -> Self {
        // A restarted host comes back with what the one it replaced showed,
        // exactly like a restarted window.
        let (app, boot) = runtime.enter(|| {
            let (mut app, boot) = App::boot(crate::app::restore::from_env());
            app.set_headless();
            (app, boot)
        });
        let now = Instant::now();
        let mut host = Self {
            app,
            runtime,
            renderer,
            cache: user_interface::Cache::default(),
            inputs,
            window: window::Id::unique(),
            size,
            scale,
            cursor: mouse::Cursor::Unavailable,
            clipboard: MemoryClipboard::default(),
            redraw: window::RedrawRequest::NextFrame,
            last_frame: now,
            last_activity: now,
            idle_waiters: Vec::new(),
            queued: VecDeque::new(),
            quit_replies: Vec::new(),
            exit: None,
            restart: None,
            pressed: false,
            recording: None,
        };
        // Subscribed before the first event, so `listen_with` hears the size.
        host.resubscribe();
        host.spawn(boot);
        host.deliver(vec![Event::Window(window::Event::Opened {
            position: None,
            size,
        })]);
        host
    }

    /// Serves inputs until the app exits; returns the exit code.
    fn run(&mut self) -> i32 {
        loop {
            let input = match self.queued.pop_front() {
                Some((command, reply)) => Some(Input::Control(command, reply)),
                None => match self.wakeup() {
                    Some(after) => match self.inputs.recv_timeout(after) {
                        Ok(input) => Some(input),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return 1,
                    },
                    None => match self.inputs.recv() {
                        Ok(input) => Some(input),
                        Err(mpsc::RecvError) => return 1,
                    },
                },
            };
            match input {
                Some(Input::Host(event)) => self.host_event(event),
                Some(Input::Control(command, reply)) => self.control(command, reply),
                None => {}
            }
            if let Some(code) = self.exit {
                return code;
            }
            self.frame();
            self.capture();
            self.answer_idle_waiters();
        }
    }

    /// Answers everything still waiting once the loop has ended.
    fn close(&mut self) {
        for reply in self.quit_replies.drain(..) {
            let _ = reply.send("ok".to_owned());
        }
        for waiter in self.idle_waiters.drain(..) {
            let _ = waiter.reply.send("err exiting".to_owned());
        }
        for (_, reply) in self.queued.drain(..) {
            let _ = reply.send("err exiting".to_owned());
        }
        if let Some(recording) = self.recording.take() {
            let path = recording.path.clone();
            eprintln!(
                "[remote] recording {}: {}",
                path.display(),
                recording.finish()
            );
        }
        // Dropping the receiver makes every later command fail at once
        // instead of waiting for a loop that is gone.
        let (_, closed) = mpsc::channel();
        let inputs = std::mem::replace(&mut self.inputs, closed);
        for input in inputs.try_iter() {
            if let Input::Control(_, reply) = input {
                let _ = reply.send("err exiting".to_owned());
            }
        }
    }

    fn host_event(&mut self, event: HostEvent) {
        match event {
            HostEvent::Message(message) => {
                if !is_heartbeat(&message) {
                    self.last_activity = Instant::now();
                }
                self.update(message);
            }
            HostEvent::Action(action) => {
                self.last_activity = Instant::now();
                self.perform(action);
            }
        }
    }

    /// Runs the app's `update`, starts the task it returns, resubscribes,
    /// and asks for a frame the way a window redraws after an update.
    fn update(&mut self, message: Message) {
        if let Message::RestartExec(restore) = message {
            self.restart = Some(restore);
            self.exit = Some(1);
            return;
        }
        let task = self.runtime.enter(|| self.app.update(message));
        self.spawn(task);
        self.resubscribe();
        self.redraw = window::RedrawRequest::NextFrame;
    }

    fn spawn(&mut self, task: Task<Message>) {
        if let Some(stream) = task::into_stream(task) {
            self.runtime.run(stream.map(HostEvent::Action).boxed());
        }
    }

    fn resubscribe(&mut self) {
        let recipes = subscription::into_recipes(
            self.runtime
                .enter(|| self.app.subscription().map(HostEvent::Message)),
        );
        self.runtime.track(recipes);
    }

    /// Feeds events to the interface, broadcasts them to the subscriptions
    /// with the status the interface gave each, then applies the messages
    /// the widgets published.
    fn deliver(&mut self, events: Vec<Event>) {
        let mut is_frame = false;
        for event in &events {
            match event {
                Event::Mouse(mouse::Event::CursorMoved { position }) => {
                    self.cursor = mouse::Cursor::Available(*position);
                }
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                    self.pressed = true;
                }
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                    self.pressed = false;
                }
                Event::Window(window::Event::RedrawRequested(_)) => is_frame = true,
                _ => {}
            }
        }
        let mut messages = Vec::new();
        let mut ui = UserInterface::build(
            self.app.view(),
            self.size,
            std::mem::take(&mut self.cache),
            &mut self.renderer,
        );
        let (state, statuses) = ui.update(
            &events,
            self.cursor,
            &mut self.renderer,
            &mut self.clipboard,
            &mut messages,
        );
        self.cache = ui.into_cache();

        let wanted = match state {
            user_interface::State::Updated { redraw_request, .. } => redraw_request,
            user_interface::State::Outdated => window::RedrawRequest::NextFrame,
        };
        // A frame answers the request that was pending; any other event can
        // only bring the next frame closer.
        self.redraw = if is_frame {
            wanted
        } else {
            self.redraw.min(wanted)
        };

        for (event, status) in events.into_iter().zip(statuses) {
            self.runtime.broadcast(subscription::Event::Interaction {
                window: self.window,
                event,
                status,
            });
        }
        for message in messages {
            self.update(message);
        }
    }

    /// Delivers `RedrawRequested` when the interface asked for it.
    fn frame(&mut self) {
        let now = Instant::now();
        let due = match self.redraw {
            window::RedrawRequest::NextFrame => now >= self.last_frame + FRAME,
            window::RedrawRequest::At(at) => at <= now,
            window::RedrawRequest::Wait => false,
        };
        if due {
            self.last_frame = now;
            self.deliver(vec![Event::Window(window::Event::RedrawRequested(now))]);
        }
    }

    /// How long the loop may sleep before a frame, a recorded frame or a
    /// `wait-idle` is due. `None` sleeps until the next input.
    fn wakeup(&self) -> Option<Duration> {
        let now = Instant::now();
        let frame = match self.redraw {
            window::RedrawRequest::NextFrame => Some(self.last_frame + FRAME),
            window::RedrawRequest::At(at) => Some(at),
            window::RedrawRequest::Wait => None,
        };
        // Once a waiter is quiet, only a pending frame holds its answer back,
        // and that frame wakes the loop by itself.
        let waiters = self.idle_waiters.iter().map(|w| {
            let quiet = self.last_activity.max(w.since) + w.quiet;
            let give_up = w.since + IDLE_TIMEOUT;
            if quiet <= now {
                give_up
            } else {
                quiet.min(give_up)
            }
        });
        let capture = self.recording.as_ref().map(Recording::next_frame);
        frame
            .into_iter()
            .chain(capture)
            .chain(waiters)
            .min()
            .map(|at| at.saturating_duration_since(now))
    }

    fn answer_idle_waiters(&mut self) {
        if self.idle_waiters.is_empty() {
            return;
        }
        let now = Instant::now();
        let redraw_due = match self.redraw {
            window::RedrawRequest::NextFrame => true,
            window::RedrawRequest::At(at) => at <= now,
            window::RedrawRequest::Wait => false,
        };
        let last_activity = self.last_activity;
        self.idle_waiters.retain(|waiter| {
            let answer = if !redraw_due && now >= last_activity.max(waiter.since) + waiter.quiet {
                "ok idle"
            } else if now >= waiter.since + IDLE_TIMEOUT {
                "err timeout"
            } else {
                return true;
            };
            let _ = waiter.reply.send(answer.to_owned());
            false
        });
    }

    fn control(&mut self, command: Command, reply: mpsc::Sender<String>) {
        let answer = match command {
            Command::Size => format!("ok {} {} {}", self.size.width, self.size.height, self.scale),
            Command::Move { to, over } if over.is_zero() => {
                self.pointer(vec![moved(to)], Duration::ZERO)
            }
            Command::Move { to, over } => {
                let from = self.cursor.position().unwrap_or(to);
                let steps = u32::try_from(over.as_millis() / 16)
                    .unwrap_or(u32::MAX)
                    .max(1);
                self.pointer(path(from, to, steps).collect(), over / steps)
            }
            Command::Down(button) => self.pointer(vec![pressed(button)], Duration::ZERO),
            Command::Up(button) => self.pointer(vec![released(button)], Duration::ZERO),
            Command::Click(at, button, modifiers) => {
                let mut events = vec![moved(at), pressed(button), released(button)];
                if !modifiers.is_empty() {
                    events.insert(
                        0,
                        Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)),
                    );
                    events.push(Event::Keyboard(keyboard::Event::ModifiersChanged(
                        Modifiers::empty(),
                    )));
                }
                self.pointer(events, Duration::ZERO)
            }
            Command::DoubleClick(at) => self.pointer(
                vec![
                    moved(at),
                    pressed(mouse::Button::Left),
                    released(mouse::Button::Left),
                    pressed(mouse::Button::Left),
                    released(mouse::Button::Left),
                ],
                Duration::ZERO,
            ),
            Command::Drag {
                from,
                to,
                steps,
                over,
            } => {
                let events: Vec<Event> = [moved(from), pressed(mouse::Button::Left)]
                    .into_iter()
                    .chain(path(from, to, steps))
                    .chain([released(mouse::Button::Left)])
                    .collect();
                self.pointer(events, over / steps)
            }
            Command::Scroll { at, dx, dy } => self.pointer(
                vec![
                    moved(at),
                    Event::Mouse(mouse::Event::WheelScrolled {
                        delta: mouse::ScrollDelta::Lines { x: dx, y: dy },
                    }),
                ],
                Duration::ZERO,
            ),
            Command::Key(keystroke) => self.input(key_events(&keystroke)),
            Command::Type(text) => {
                let events: Vec<Event> = text
                    .chars()
                    .flat_map(|c| key_events(&Keystroke::character(c, Modifiers::empty())))
                    .collect();
                self.input(events)
            }
            Command::Find(text) => self.find(&text),
            Command::Screenshot(path) => {
                let shot = self.screenshot();
                match write_png(&path, &shot) {
                    Ok(()) => format!(
                        "ok {} {}x{}",
                        path.display(),
                        shot.size.width,
                        shot.size.height
                    ),
                    Err(e) => format!("err {}: {e}", path.display()),
                }
            }
            Command::Record(path) => self.record(path),
            Command::RecordStop => match self.recording.take() {
                Some(recording) => recording.finish(),
                None => "err not recording".to_owned(),
            },
            // A video has one frame size.
            Command::Resize(_) | Command::Scale(_) if self.recording.is_some() => {
                "err stop the recording first".to_owned()
            }
            Command::Resize(size) => {
                self.size = size;
                self.input([Event::Window(window::Event::Resized(size))])
            }
            Command::Scale(scale) => {
                self.scale = scale;
                self.input([
                    Event::Window(window::Event::Rescaled(scale)),
                    Event::Window(window::Event::Resized(self.size)),
                ])
            }
            Command::Clip => match self.clipboard.read(clipboard::Kind::Standard) {
                Some(text) => format!("ok {text}"),
                None => "ok".to_owned(),
            },
            Command::ClipSet(text) => {
                self.clipboard.write(clipboard::Kind::Standard, text);
                "ok".to_owned()
            }
            Command::WaitIdle(quiet) => {
                self.idle_waiters.push(IdleWaiter {
                    quiet,
                    since: Instant::now(),
                    reply,
                });
                return;
            }
            Command::Restart => {
                self.update(Message::Restart);
                "ok restarting".to_owned()
            }
            Command::Quit => {
                // The same path as the window's close button: flush, close
                // the QUIC connections, then `Message::Exit` ends the loop.
                self.quit_replies.push(reply);
                self.update(Message::CloseRequested);
                return;
            }
        };
        let _ = reply.send(answer);
    }

    /// Delivers input one event at a time, as a window would across frames,
    /// then lets the answers from subscriptions arrive before replying.
    fn input(&mut self, events: impl IntoIterator<Item = Event>) -> String {
        for event in events {
            self.deliver(vec![event]);
            if self.exit.is_some() {
                break;
            }
        }
        self.settle();
        "ok".to_owned()
    }

    /// Delivers pointer events with the loop running in between, the way a
    /// hand moves a mouse: a drag starts through a subscription message, so
    /// the press must be processed before the first move arrives. A zero
    /// `interval` settles after each event; otherwise event `i` is followed
    /// by running the loop until `start + interval * (i + 1)`.
    fn pointer(&mut self, events: Vec<Event>, interval: Duration) -> String {
        let start = Instant::now();
        for (i, event) in (1u32..).zip(events) {
            self.deliver(vec![event]);
            if self.exit.is_some() {
                break;
            }
            if interval.is_zero() {
                self.settle();
            } else {
                self.pump(start + interval * i);
            }
        }
        self.settle();
        "ok".to_owned()
    }

    fn settle(&mut self) {
        let start = Instant::now();
        while self.exit.is_none() && start.elapsed() < SETTLE_MAX {
            match self.inputs.recv_timeout(SETTLE) {
                Ok(Input::Host(event)) => self.host_event(event),
                Ok(Input::Control(command, reply)) => self.queued.push_back((command, reply)),
                Err(_) => break,
            }
            self.frame();
            self.capture();
        }
    }

    /// Runs the loop until `until`: host events, frames and recorded
    /// frames, with control commands queued for later.
    fn pump(&mut self, until: Instant) {
        while self.exit.is_none() {
            let now = Instant::now();
            if now >= until {
                break;
            }
            let wait = self
                .wakeup()
                .map_or(until - now, |after| after.min(until - now));
            match self.inputs.recv_timeout(wait) {
                Ok(Input::Host(event)) => self.host_event(event),
                Ok(Input::Control(command, reply)) => self.queued.push_back((command, reply)),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.frame();
            self.capture();
        }
    }

    /// Starts ffmpeg on a raw RGBA stream of the frames `capture` renders.
    fn record(&mut self, path: PathBuf) -> String {
        if let Some(recording) = &self.recording {
            return format!("err already recording {}", recording.path.display());
        }
        let size = self.physical_size();
        let spawned = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "rawvideo"])
            .args(["-pixel_format", "rgba", "-video_size"])
            .arg(format!("{}x{}", size.width, size.height))
            .arg("-framerate")
            .arg(RECORD_FPS.to_string())
            .args(["-i", "-", "-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2"])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20"])
            .args(["-pix_fmt", "yuv420p", "-movflags", "+faststart"])
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => return format!("err ffmpeg: {e}"),
        };
        let Some(mut stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return "err ffmpeg: no stdin".to_owned();
        };
        let (frames, received) = mpsc::sync_channel::<Arc<Vec<u8>>>(60);
        let writer = std::thread::spawn(move || {
            for frame in received {
                stdin.write_all(&frame).map_err(|e| e.to_string())?;
            }
            Ok(())
        });
        let reply = format!(
            "ok recording {} {}x{}",
            path.display(),
            size.width,
            size.height
        );
        self.recording = Some(Recording {
            path,
            started: Instant::now(),
            written: 0,
            frames,
            writer,
            child,
        });
        self.capture();
        reply
    }

    /// Sends the frame a recording is due, repeated for every slot a late
    /// capture missed, so the video keeps wall-clock time.
    fn capture(&mut self) {
        let now = Instant::now();
        match &self.recording {
            Some(recording) if now >= recording.next_frame() => {}
            _ => return,
        }
        let (mut rgba, size) = self.render();
        if let Some(at) = self.cursor.position() {
            draw_pointer(&mut rgba, size, at, self.scale, self.pressed);
        }
        let Some(recording) = &mut self.recording else {
            return;
        };
        let frame = Arc::new(rgba);
        let slots =
            ((now - recording.started).as_secs_f64() * f64::from(RECORD_FPS)).floor() as u64 + 1;
        for _ in recording.written..slots {
            // A failed writer is reported by `finish`.
            let _ = recording.frames.send(frame.clone());
        }
        recording.written = recording.written.max(slots);
    }

    fn perform(&mut self, action: Action<Message>) {
        match action {
            Action::Output(message) => self.update(message),
            Action::LoadFont { bytes, channel } => {
                font_system().write().expect("font system").load_font(bytes);
                let _ = channel.send(Ok(()));
            }
            Action::Widget(operation) => self.operate(operation),
            Action::Clipboard(action) => match action {
                iced_runtime::clipboard::Action::Read { target, channel } => {
                    let _ = channel.send(self.clipboard.read(target));
                }
                iced_runtime::clipboard::Action::Write { target, contents } => {
                    self.clipboard.write(target, contents);
                }
            },
            Action::Window(action) => self.window_action(action),
            Action::System(_) | Action::Image(_) | Action::Reload => {}
            Action::Exit => self.exit = Some(0),
        }
    }

    fn operate(&mut self, operation: Box<dyn Operation>) {
        let mut ui = UserInterface::build(
            self.app.view(),
            self.size,
            std::mem::take(&mut self.cache),
            &mut self.renderer,
        );
        let mut operation = Some(operation);
        while let Some(mut current) = operation.take() {
            ui.operate(&self.renderer, &mut current);
            if let Outcome::Chain(next) = current.finish() {
                operation = Some(next);
            }
        }
        self.cache = ui.into_cache();
        // A focus or scroll changes what is drawn.
        self.redraw = window::RedrawRequest::NextFrame;
    }

    fn window_action(&mut self, action: iced_runtime::window::Action) {
        use iced_runtime::window::Action as Window;
        match action {
            Window::Open(id, _, sender) => {
                let _ = sender.send(id);
            }
            Window::GetOldest(sender) | Window::GetLatest(sender) => {
                let _ = sender.send(Some(self.window));
            }
            Window::GetSize(_, sender) => {
                let _ = sender.send(self.size);
            }
            Window::GetMaximized(_, sender) => {
                let _ = sender.send(false);
            }
            Window::GetMinimized(_, sender) => {
                let _ = sender.send(None);
            }
            Window::GetPosition(_, sender) => {
                let _ = sender.send(Some(Point::ORIGIN));
            }
            Window::GetScaleFactor(_, sender) => {
                let _ = sender.send(self.scale);
            }
            Window::GetMode(_, sender) => {
                let _ = sender.send(window::Mode::Windowed);
            }
            Window::Screenshot(_, sender) => {
                let _ = sender.send(self.screenshot());
            }
            _ => eprintln!("[remote] ignored window action"),
        }
    }

    /// `ok X Y W H` of the first widget whose text is exactly `text`, found
    /// the way `iced_test` finds a click target.
    fn find(&mut self, text: &str) -> String {
        let mut ui = UserInterface::build(
            self.app.view(),
            self.size,
            std::mem::take(&mut self.cache),
            &mut self.renderer,
        );
        let mut selector = Selector::find(text);
        ui.operate(&self.renderer, &mut operation::black_box(&mut selector));
        self.cache = ui.into_cache();
        match selector.finish() {
            Outcome::Some(Some(found)) => match found.visible_bounds() {
                Some(b) => format!(
                    "ok {} {} {} {} (inside the node graph these are graph layout coordinates, not screen coordinates)",
                    b.x, b.y, b.width, b.height
                ),
                None => "err not visible".to_owned(),
            },
            _ => "err not found".to_owned(),
        }
    }

    /// Draws a frame with the current cursor.
    fn screenshot(&mut self) -> window::Screenshot {
        let (rgba, size) = self.render();
        window::Screenshot::new(rgba, size, self.scale)
    }

    /// The frame size `render` produces.
    fn physical_size(&self) -> Size<u32> {
        Size::new(
            (self.size.width * self.scale).round() as u32,
            (self.size.height * self.scale).round() as u32,
        )
    }

    /// Draws a frame with the current cursor and reads it back as RGBA.
    fn render(&mut self) -> (Vec<u8>, Size<u32>) {
        let theme = self.app.theme();
        let mut messages = Vec::new();
        let mut ui = UserInterface::build(
            self.app.view(),
            self.size,
            std::mem::take(&mut self.cache),
            &mut self.renderer,
        );
        let _ = ui.update(
            &[Event::Window(
                window::Event::RedrawRequested(Instant::now()),
            )],
            self.cursor,
            &mut self.renderer,
            &mut self.clipboard,
            &mut messages,
        );
        ui.draw(
            &mut self.renderer,
            &theme,
            &renderer::Style {
                text_color: theme.extended().background.base.text,
            },
            self.cursor,
        );
        self.cache = ui.into_cache();

        let physical = self.physical_size();
        let rgba =
            Headless::screenshot(&mut self.renderer, physical, self.scale, Color::TRANSPARENT);
        for message in messages {
            self.update(message);
        }
        (rgba, physical)
    }
}

/// Periodic messages that mean "time passed", not "something happened":
/// with a runner connected the sync poll alone would keep `wait-idle` from
/// ever answering.
fn is_heartbeat(message: &Message) -> bool {
    matches!(message, Message::Tick | Message::SyncPoll)
}

fn moved(position: Point) -> Event {
    Event::Mouse(mouse::Event::CursorMoved { position })
}

fn pressed(button: mouse::Button) -> Event {
    Event::Mouse(mouse::Event::ButtonPressed(button))
}

fn released(button: mouse::Button) -> Event {
    Event::Mouse(mouse::Event::ButtonReleased(button))
}

/// `steps` evenly spaced moves from `from` (exclusive) to `to` (inclusive).
fn path(from: Point, to: Point, steps: u32) -> impl Iterator<Item = Event> {
    (1..=steps).map(move |i| {
        let t = i as f32 / steps as f32;
        moved(Point::new(
            from.x + (to.x - from.x) * t,
            from.y + (to.y - from.y) * t,
        ))
    })
}

/// The arrow a recording draws, tip at the top left: `X` outline, `.` fill,
/// space transparent. One character is one logical pixel.
const POINTER: [&str; 17] = [
    "X",
    "XX",
    "X.X",
    "X..X",
    "X...X",
    "X....X",
    "X.....X",
    "X......X",
    "X.......X",
    "X........X",
    "X.....XXXXX",
    "X..X..X",
    "X.X X..X",
    "XX  X..X",
    "X    X..X",
    "     X..X",
    "      XX",
];

/// Paints the pointer with its tip at the logical point `at`; `pressed`
/// adds a ring around the tip, so a drag reads as a drag.
fn draw_pointer(rgba: &mut [u8], size: Size<u32>, at: Point, scale: f32, pressed: bool) {
    const OUTLINE: [u8; 4] = [0, 0, 0, 255];
    const FILL: [u8; 4] = [255, 255, 255, 255];
    const RING: [u8; 4] = [255, 196, 0, 255];
    let tip = (at.x * scale, at.y * scale);
    let mut paint = |x: i64, y: i64, color: [u8; 4]| {
        if x < 0 || y < 0 || x >= i64::from(size.width) || y >= i64::from(size.height) {
            return;
        }
        let i = (y as usize * size.width as usize + x as usize) * 4;
        if let Some(pixel) = rgba.get_mut(i..i + 4) {
            pixel.copy_from_slice(&color);
        }
    };
    if pressed {
        let radius = 9.0 * scale;
        let half = scale;
        let reach = (radius + half).ceil() as i64;
        let (cx, cy) = (tip.0.round() as i64, tip.1.round() as i64);
        for dy in -reach..=reach {
            for dx in -reach..=reach {
                let distance = ((dx * dx + dy * dy) as f32).sqrt();
                if (distance - radius).abs() <= half {
                    paint(cx + dx, cy + dy, RING);
                }
            }
        }
    }
    let block = scale.ceil() as i64;
    for (row, line) in (0i64..).zip(POINTER) {
        for (col, c) in (0i64..).zip(line.chars()) {
            let color = match c {
                'X' => OUTLINE,
                '.' => FILL,
                _ => continue,
            };
            let x0 = (tip.0 + col as f32 * scale).floor() as i64;
            let y0 = (tip.1 + row as f32 * scale).floor() as i64;
            for y in y0..y0 + block {
                for x in x0..x0 + block {
                    paint(x, y, color);
                }
            }
        }
    }
}

/// A key press and release, bracketed by the modifier changes a keyboard
/// reports around them.
fn key_events(keystroke: &Keystroke) -> [Event; 4] {
    let physical_key =
        keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified);
    [
        Event::Keyboard(keyboard::Event::ModifiersChanged(keystroke.modifiers)),
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: keystroke.key.clone(),
            modified_key: keystroke.modified_key.clone(),
            physical_key,
            location: keyboard::Location::Standard,
            modifiers: keystroke.modifiers,
            text: keystroke.text.map(|c| c.to_string().into()),
            repeat: false,
        }),
        Event::Keyboard(keyboard::Event::KeyReleased {
            key: keystroke.key.clone(),
            modified_key: keystroke.modified_key.clone(),
            physical_key,
            location: keyboard::Location::Standard,
            modifiers: keystroke.modifiers,
        }),
        Event::Keyboard(keyboard::Event::ModifiersChanged(Modifiers::empty())),
    ]
}

fn write_png(path: &Path, shot: &window::Screenshot) -> Result<(), String> {
    let file = File::create(path).map_err(|e| e.to_string())?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), shot.size.width, shot.size.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer
        .write_image_data(&shot.rgba)
        .map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())
}

/// The clipboard a window would share with the desktop, kept in memory so a
/// headless run never touches the user's.
#[derive(Default)]
struct MemoryClipboard {
    standard: Option<String>,
    primary: Option<String>,
}

impl Clipboard for MemoryClipboard {
    fn read(&self, kind: clipboard::Kind) -> Option<String> {
        match kind {
            clipboard::Kind::Standard => self.standard.clone(),
            clipboard::Kind::Primary => self.primary.clone(),
        }
    }

    fn write(&mut self, kind: clipboard::Kind, contents: String) {
        match kind {
            clipboard::Kind::Standard => self.standard = Some(contents),
            clipboard::Kind::Primary => self.primary = Some(contents),
        }
    }
}

/// Binds the control socket, replacing a stale one a killed host left behind.
fn bind(path: &Path) -> std::io::Result<UnixListener> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    UnixListener::bind(path)
}

/// One thread per connection: a command may wait (`wait-idle`, `quit`), and
/// another client must not queue behind it.
fn accept(listener: UnixListener, inputs: &mpsc::Sender<Input>, inflight: &Arc<Inflight>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let inputs = inputs.clone();
        let guard = Inflight::enter(inflight);
        std::thread::spawn(move || {
            serve(&stream, &inputs);
            drop(guard);
        });
    }
}

/// Reads one command line, has the loop answer it, writes the reply.
fn serve(stream: &UnixStream, inputs: &mpsc::Sender<Input>) {
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).is_err() {
        return;
    }
    let reply = match Command::parse(&line) {
        Err(e) => format!("err {e}"),
        Ok(command) => {
            let (reply_tx, reply_rx) = mpsc::channel();
            if inputs.send(Input::Control(command, reply_tx)).is_err() {
                "err exiting".to_owned()
            } else {
                reply_rx.recv().unwrap_or_else(|_| "err exiting".to_owned())
            }
        }
    };
    let mut stream = stream;
    let _ = writeln!(stream, "{reply}");
}

/// Connections that have not written their reply yet, so the exit can let
/// the last one (`quit`'s own) reach its client.
#[derive(Default)]
struct Inflight {
    count: Mutex<usize>,
    idle: Condvar,
}

struct InflightGuard(Arc<Inflight>);

impl Inflight {
    fn enter(this: &Arc<Self>) -> InflightGuard {
        *this.count.lock().expect("inflight count") += 1;
        InflightGuard(this.clone())
    }

    fn drain(&self, limit: Duration) {
        let count = self.count.lock().expect("inflight count");
        let _ = self
            .idle
            .wait_timeout_while(count, limit, |count| *count > 0);
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        *self.0.count.lock().expect("inflight count") -= 1;
        self.0.idle.notify_all();
    }
}
