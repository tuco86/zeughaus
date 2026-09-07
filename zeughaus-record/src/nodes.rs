//! The two nodes: one writes a dataset, one plays it back.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ::image::ImageEncoder;

use zeughaus_core::*;

/// The index of a recording: one JSON object per line, in write order.
pub const INDEX_FILE: &str = "index.jsonl";

/// The file name of frame `seq`. Zero-padded so a directory listing is in
/// order, which is what makes a recording readable without this crate.
pub fn frame_file(seq: u64) -> String {
    format!("{seq:06}.png")
}

/// A session name from the wall clock: whole unix seconds.
///
/// Seconds rather than a formatted date: a name has to sort, be typeable and
/// contain nothing a filesystem argues about, and it is only ever compared to
/// other recordings on the same machine.
pub fn timestamp_name() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// A scalar as JSON; anything else as the text its `Repr` displays.
///
/// A recording is data to read later, so a value that has no JSON form still
/// has to leave something in the line: the display string is what the editor
/// shows for it, which is the most honest stand-in available.
fn value_json(value: &Value) -> serde_json::Value {
    match value.repr() {
        Repr::Bool(v) => serde_json::Value::Bool(v),
        Repr::Int(v) => serde_json::Value::from(v),
        Repr::Float(v) => serde_json::Number::from_f64(v)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Repr::Str(v) => serde_json::Value::from(v),
        other => serde_json::Value::from(other.to_string()),
    }
}

fn failed(message: impl Into<String>) -> ZeughausError {
    ZeughausError::ExecutionFailed(message.into())
}

/// Reads a parameter's text, whatever scalar form it arrives in: settings
/// travel as strings, but a value wired in from elsewhere may be a number.
fn text_of(value: &Value) -> Option<String> {
    match value.repr() {
        Repr::Str(s) => Some(s.to_string()),
        Repr::Int(v) => Some(v.to_string()),
        Repr::Float(v) => Some(v.to_string()),
        Repr::Bool(v) => Some(v.to_string()),
        _ => None,
    }
}

/// Writes every frame it is triggered with, plus whatever values are wired
/// beside it, into `<dir>/<session>/`.
pub struct RecorderNode {
    dir: String,
    session: String,
    /// The session actually being written, which is `session` unless that was
    /// left empty and the first frame named one.
    active: Option<String>,
    seq: u64,
    pins: Vec<PinDefinition>,
    /// How many value inputs the pin list offers. Always one more than are
    /// connected, so there is somewhere to drop the next wire.
    slots: usize,
}

impl Default for RecorderNode {
    fn default() -> Self {
        Self::new()
    }
}

impl RecorderNode {
    pub const DEFAULT_DIR: &'static str = "recordings";

    pub fn new() -> Self {
        let mut node = Self {
            dir: Self::DEFAULT_DIR.to_string(),
            session: String::new(),
            active: None,
            seq: 0,
            pins: Vec::new(),
            slots: 1,
        };
        node.rebuild_pins();
        node
    }

    /// The frame trigger, `slots` value inputs, and what was written.
    fn rebuild_pins(&mut self) {
        let mut pins = Vec::with_capacity(self.slots + 4);
        pins.push(PinDefinition::input(
            "frame",
            Ty::of::<Image>(),
            PinKind::Trigger,
        ));
        for slot in 0..self.slots {
            pins.push(PinDefinition::input(
                format!("v{slot}"),
                Ty::Any,
                PinKind::Sample,
            ));
        }
        pins.push(PinDefinition::output("count", Ty::Int));
        pins.push(PinDefinition::output("session", Ty::Str));
        pins.push(PinDefinition::output("path", Ty::Str));
        self.pins = pins;
    }

    /// The directory this recording is written to, once a session is decided.
    pub fn session_dir(&self) -> Option<PathBuf> {
        self.active
            .as_ref()
            .map(|session| Path::new(&self.dir).join(session))
    }

    /// How many frames have been written.
    pub fn count(&self) -> u64 {
        self.seq
    }

    /// Forgets which recording is being written, so the next frame decides the
    /// directory again and numbering starts at one.
    fn restart(&mut self) {
        self.active = None;
        self.seq = 0;
    }

    fn emit_state(&self, ctx: &mut NodeContext) {
        ctx.emit_typed("count", self.seq as i64);
        ctx.emit_typed("session", self.active.clone().unwrap_or_default());
        ctx.emit_typed(
            "path",
            self.session_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        ctx.flush();
    }

    /// Writes one frame and its line. Kept apart from `execute` so every IO
    /// failure has one place to turn into a node error while the counters stay
    /// where they were.
    fn write_frame(
        &mut self,
        frame: &Image,
        values: serde_json::Map<String, serde_json::Value>,
    ) -> Result<()> {
        let session = match &self.active {
            Some(session) => session.clone(),
            None => {
                let session = if self.session.trim().is_empty() {
                    timestamp_name()
                } else {
                    self.session.trim().to_string()
                };
                self.active = Some(session.clone());
                session
            }
        };
        let dir = Path::new(&self.dir).join(&session);
        fs::create_dir_all(&dir)
            .map_err(|e| failed(format!("cannot create {}: {e}", dir.display())))?;

        let seq = self.seq + 1;
        let file = frame_file(seq);
        // Encoded straight out of the frame's shared buffer: an `RgbaImage`
        // would need its own copy of it, which for a 4K frame is 33 MB per
        // written frame for nothing. `::image` because `zeughaus_core::*`
        // brings in a module of the same name.
        let target = dir.join(&file);
        let sink = io::BufWriter::new(
            fs::File::create(&target).map_err(|e| failed(format!("cannot write {file}: {e}")))?,
        );
        ::image::codecs::png::PngEncoder::new(sink)
            .write_image(
                frame.rgba(),
                frame.width(),
                frame.height(),
                ::image::ExtendedColorType::Rgba8,
            )
            .map_err(|e| failed(format!("cannot encode {file}: {e}")))?;

        let line = serde_json::json!({
            "seq": seq,
            "ts": now_seconds(),
            "file": file,
            "width": frame.width(),
            "height": frame.height(),
            "values": serde_json::Value::Object(values),
        });
        let mut index = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(INDEX_FILE))
            .map_err(|e| failed(format!("cannot open {INDEX_FILE}: {e}")))?;
        writeln!(index, "{line}")
            .map_err(|e| failed(format!("cannot append to {INDEX_FILE}: {e}")))?;

        // Only after the frame and its line are both on disk: a counter that
        // moved on a failed write would leave a gap in the sequence.
        self.seq = seq;
        Ok(())
    }
}

impl ExecutableNode for RecorderNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        if !inputs.changed("frame") {
            // A pass this node runs in for someone else's reason is not a new
            // frame: writing the same one again would double every recording
            // whose value inputs change faster than its source.
            self.emit_state(ctx);
            return Ok(());
        }
        let frame = inputs
            .get_value("frame")
            .and_then(|v| v.downcast_ref::<Image>())
            .ok_or_else(|| failed("no frame wired"))?
            .clone();

        let values: serde_json::Map<String, serde_json::Value> = (0..self.slots)
            .filter_map(|slot| {
                let name = format!("v{slot}");
                let value = inputs.get_value(&name)?;
                Some((name, value_json(value)))
            })
            .collect();

        self.write_frame(&frame, values)?;
        self.emit_state(ctx);
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![
            SettingDef::new("dir", self.dir.clone()),
            SettingDef::new("session", self.session.clone()).placeholder("new on first frame"),
        ]
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        let Some(text) = text_of(&value) else {
            return Ok(());
        };
        match name {
            // Either setting decides where the frames go, so either one is a
            // new recording: leaving `seq` alone would continue numbering into
            // a fresh index under the new root, splitting one nominal session
            // across two directories.
            "dir" => {
                let dir = text.trim().to_string();
                if dir != self.dir {
                    self.dir = dir;
                    self.restart();
                }
            }
            "session" => {
                let session = text.trim().to_string();
                if session != self.session {
                    self.session = session;
                    self.restart();
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// One value input per wire, plus exactly one spare.
    ///
    /// The spare is the whole rule: there has to be somewhere to drop the next
    /// wire, and no more than that, or the node grows a column of pins nobody
    /// uses. Only a real change is reported, because the host redraws and
    /// re-syncs on every `true`.
    fn sync_pins(&mut self, connected: &[PinBinding<'_>]) -> bool {
        let used = connected
            .iter()
            .filter(|binding| {
                binding
                    .name
                    .strip_prefix('v')
                    .is_some_and(|slot| slot.parse::<usize>().is_ok())
            })
            .count();
        let slots = used + 1;
        if slots == self.slots {
            return false;
        }
        self.slots = slots;
        self.rebuild_pins();
        true
    }
}

/// One line of an index, as the player reads it.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    seq: i64,
    ts: f64,
    file: String,
    values: String,
}

/// Replays a recording: one frame per tick, in write order.
pub struct PlayerNode {
    dir: String,
    session: String,
    hz: f64,
    looping: bool,
    pins: Vec<PinDefinition>,
    /// The index as loaded, and `None` while it still has to be read. Read
    /// once per session rather than per tick: the file is the recording's
    /// table of contents and does not change while it is being played.
    entries: Option<Vec<Entry>>,
    /// Which session `entries` was loaded from, so a changed setting reloads.
    loaded: Option<String>,
    next: usize,
}

impl Default for PlayerNode {
    fn default() -> Self {
        Self::new()
    }
}

impl PlayerNode {
    pub const DEFAULT_DIR: &'static str = "recordings";
    pub const DEFAULT_HZ: f64 = 10.0;
    pub const MIN_HZ: f64 = 0.1;
    pub const MAX_HZ: f64 = 120.0;

    pub fn new() -> Self {
        Self {
            dir: Self::DEFAULT_DIR.to_string(),
            session: String::new(),
            hz: Self::DEFAULT_HZ,
            looping: true,
            pins: vec![
                PinDefinition::output("frame", Ty::of::<Image>()),
                PinDefinition::output("seq", Ty::Int),
                PinDefinition::output("ts", Ty::Float),
                PinDefinition::output("values", Ty::Str),
            ],
            entries: None,
            loaded: None,
            next: 0,
        }
    }

    fn session_dir(&self) -> PathBuf {
        Path::new(&self.dir).join(self.session.trim())
    }

    /// Reads the index unless it is already loaded for this session.
    ///
    /// A malformed line is skipped rather than refused: a recording whose last
    /// line was cut short by a crash is still worth playing.
    fn load_index(&mut self) -> Result<()> {
        if self.session.trim().is_empty() {
            return Err(failed("no session: name the recording to play"));
        }
        if self.loaded.as_deref() == Some(self.session.trim()) {
            return Ok(());
        }
        let path = self.session_dir().join(INDEX_FILE);
        let text = fs::read_to_string(&path)
            .map_err(|e| failed(format!("cannot read {}: {e}", path.display())))?;
        let entries: Vec<Entry> = text
            .lines()
            .filter_map(|line| {
                let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
                Some(Entry {
                    seq: parsed.get("seq")?.as_i64()?,
                    ts: parsed.get("ts").and_then(serde_json::Value::as_f64)?,
                    file: parsed.get("file")?.as_str()?.to_string(),
                    values: parsed
                        .get("values")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}))
                        .to_string(),
                })
            })
            .collect();
        if entries.is_empty() {
            return Err(failed(format!("{} names no frames", path.display())));
        }
        self.entries = Some(entries);
        self.loaded = Some(self.session.trim().to_string());
        self.next = 0;
        Ok(())
    }
}

impl ExecutableNode for PlayerNode {
    fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let dir = self.session_dir();
        self.load_index()?;
        let total = self.entries.as_ref().map_or(0, Vec::len);
        if self.next >= total {
            if !self.looping {
                // Emitting nothing is the end of the recording: the pins go
                // empty rather than repeating the last frame forever.
                return Ok(());
            }
            self.next = 0;
        }
        let entry = self.entries.as_ref().expect("the index is loaded")[self.next].clone();
        self.next += 1;

        let path = dir.join(&entry.file);
        let decoded = ::image::open(&path)
            .map_err(|e| failed(format!("cannot read {}: {e}", path.display())))?
            .into_rgba8();
        let (width, height) = (decoded.width(), decoded.height());
        let frame = Image::from_rgba(width, height, decoded.into_raw());

        ctx.emit("frame", Value::new(frame));
        ctx.emit_typed("seq", entry.seq);
        ctx.emit_typed("ts", entry.ts);
        ctx.emit_typed("values", entry.values);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![
            SettingDef::new("dir", self.dir.clone()),
            SettingDef::new("session", self.session.clone()).placeholder("1757000000"),
            SettingDef::new("hz", format!("{}", self.hz)),
            SettingDef::new("loop", format!("{}", self.looping)),
        ]
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        let Some(text) = text_of(&value) else {
            return Ok(());
        };
        match name {
            "dir" => {
                let dir = text.trim().to_string();
                if dir != self.dir {
                    self.dir = dir;
                    // The index describes files under the old directory.
                    self.loaded = None;
                }
            }
            "session" => self.session = text.trim().to_string(),
            // An unparsable or out-of-range rate keeps the previous one: a
            // half-typed number in the editor must not stop playback.
            "hz" => {
                if let Some(hz) = text.trim().parse::<f64>().ok().filter(|hz| hz.is_finite()) {
                    self.hz = hz.clamp(Self::MIN_HZ, Self::MAX_HZ);
                }
            }
            // Only explicit spellings. Reading every other text as "false"
            // meant a typo silently stopped playback after one run, with
            // nothing anywhere saying why.
            "loop" => {
                let trimmed = text.trim();
                self.looping = match trimmed.to_ascii_lowercase().as_str() {
                    "true" | "1" => true,
                    "false" | "0" => false,
                    _ => {
                        return Err(ZeughausError::ExecutionFailed(format!(
                            "loop '{trimmed}': expected true or false"
                        )));
                    }
                };
            }
            _ => {}
        }
        Ok(())
    }

    /// Playback is driven by time, like any other source: nothing upstream
    /// wakes a recording.
    fn tick_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs_f64(1.0 / self.hz))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zeughaus-record-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn set(node: &mut dyn ExecutableNode, name: &str, text: &str) {
        node.set_parameter(name, Value::new(text.to_string()))
            .expect("set");
    }

    fn ctx() -> NodeContext {
        NodeContext::new(NodeId(1), 0)
    }

    /// A 2x2 frame whose pixels are derived from `tint`, so two frames are
    /// distinguishable byte for byte.
    fn frame(tint: u8) -> Image {
        let pixels: Vec<u8> = (0..4u8)
            .flat_map(|i| [tint, tint.wrapping_add(i), 0x40, 0xff])
            .collect();
        Image::from_rgba(2, 2, pixels)
    }

    /// Only explicit spellings loop. Reading everything else as "false" made
    /// a typo look like a player that simply stops.
    #[test]
    fn the_loop_setting_accepts_only_explicit_spellings() {
        let mut node = PlayerNode::new();
        for (text, expected) in [("true", true), ("1", true), ("false", false), ("0", false)] {
            set(&mut node, "loop", text);
            assert_eq!(node.looping, expected, "{text}");
        }
        set(&mut node, "loop", "true");
        for bad in ["tru", "yes", "", "maybe"] {
            let error = node
                .set_parameter("loop", Value::new(bad.to_string()))
                .expect_err(bad)
                .to_string();
            assert!(error.contains("expected true or false"), "{error}");
            // The refusal left the accepted value in place.
            assert!(node.looping);
        }
    }

    /// The two nodes are counterparts: the pixels and the values a recording
    /// was written with are what playing it back produces.
    #[test]
    fn two_frames_written_come_back_out_of_the_player() {
        let dir = temp_dir("roundtrip");
        let dir_text = dir.to_string_lossy().into_owned();

        let mut writer = RecorderNode::new();
        set(&mut writer, "dir", &dir_text);
        set(&mut writer, "session", "run1");

        let mut written: Vec<Image> = Vec::new();
        for (index, tint) in [0x10u8, 0x90].into_iter().enumerate() {
            let image = frame(tint);
            let mut inputs = InputSet::new();
            inputs.insert("frame", Value::new(image.clone()));
            inputs.insert("v0", Value::new(7.0_f64));
            inputs.insert("v1", Value::new(format!("step{index}")));
            inputs.mark_changed("frame");
            // Both value slots have to exist for them to be recorded.
            writer.sync_pins(&[
                PinBinding {
                    name: "v0",
                    ty: &Ty::Float,
                },
                PinBinding {
                    name: "v1",
                    ty: &Ty::Str,
                },
            ]);
            let mut writing = ctx();
            writer.execute(&inputs, &mut writing).expect("write");
            let outputs = writing.take_outputs();
            assert_eq!(
                outputs.get("count").and_then(Value::downcast_ref::<i64>),
                Some(&(index as i64 + 1))
            );
            written.push(image);
        }
        assert_eq!(
            writer.session_dir().expect("session"),
            dir.join("run1"),
            "the session directory is where the frames went"
        );
        assert_eq!(fs::read_dir(dir.join("run1")).expect("dir").count(), 3);

        let mut player = PlayerNode::new();
        set(&mut player, "dir", &dir_text);
        set(&mut player, "session", "run1");
        set(&mut player, "loop", "false");

        for (index, source) in written.iter().enumerate() {
            let mut playing = ctx();
            player
                .execute(&InputSet::new(), &mut playing)
                .expect("play");
            let outputs = playing.take_outputs();
            let replayed = outputs
                .get("frame")
                .and_then(Value::downcast_ref::<Image>)
                .expect("frame");
            assert_eq!(replayed.width(), 2);
            assert_eq!(replayed.height(), 2);
            assert_eq!(
                replayed.rgba().as_ref(),
                source.rgba().as_ref(),
                "frame {index} came back with different pixels"
            );
            assert_eq!(
                outputs.get("seq").and_then(Value::downcast_ref::<i64>),
                Some(&(index as i64 + 1))
            );
            let values = outputs
                .get("values")
                .and_then(Value::downcast_ref::<String>)
                .expect("values");
            let parsed: serde_json::Value = serde_json::from_str(values).expect("json");
            assert_eq!(parsed["v0"], serde_json::json!(7.0));
            assert_eq!(parsed["v1"], serde_json::json!(format!("step{index}")));
            assert!(
                outputs
                    .get("ts")
                    .and_then(Value::downcast_ref::<f64>)
                    .is_some_and(|ts| *ts > 1_600_000_000.0),
                "the line records when the frame was taken"
            );
        }

        // Past the end with looping off: nothing is emitted rather than the
        // last frame over and over.
        let mut past_end = ctx();
        player
            .execute(&InputSet::new(), &mut past_end)
            .expect("end");
        assert!(past_end.take_outputs().is_empty());
    }

    /// Looping is what makes a recording a source: it restarts instead of
    /// running dry.
    #[test]
    fn a_looping_player_restarts_at_the_first_frame() {
        let dir = temp_dir("loop");
        let dir_text = dir.to_string_lossy().into_owned();
        let mut writer = RecorderNode::new();
        set(&mut writer, "dir", &dir_text);
        set(&mut writer, "session", "run1");
        let mut inputs = InputSet::new();
        inputs.insert("frame", Value::new(frame(0x20)));
        inputs.mark_changed("frame");
        writer.execute(&inputs, &mut ctx()).expect("write");

        let mut player = PlayerNode::new();
        set(&mut player, "dir", &dir_text);
        set(&mut player, "session", "run1");
        for _ in 0..3 {
            let mut playing = ctx();
            player
                .execute(&InputSet::new(), &mut playing)
                .expect("play");
            assert_eq!(
                playing
                    .take_outputs()
                    .get("seq")
                    .and_then(Value::downcast_ref::<i64>),
                Some(&1)
            );
        }
    }

    /// Both settings decide where the frames go, so both start a new
    /// recording. Numbering that continued into the new directory would leave
    /// one nominal session split across two roots, with an index in each that
    /// names only part of it.
    #[test]
    fn changing_the_directory_starts_a_new_recording() {
        let first = temp_dir("dir-a");
        let second = temp_dir("dir-b");
        let mut writer = RecorderNode::new();
        set(&mut writer, "session", "run1");

        let mut inputs = InputSet::new();
        inputs.insert("frame", Value::new(frame(0x40)));
        inputs.mark_changed("frame");

        for dir in [&first, &second] {
            set(&mut writer, "dir", &dir.to_string_lossy());
            let mut writing = ctx();
            writer.execute(&inputs, &mut writing).expect("write");
            assert_eq!(
                writing
                    .take_outputs()
                    .get("count")
                    .and_then(Value::downcast_ref::<i64>),
                Some(&1),
                "each directory is its own recording"
            );
            assert!(dir.join("run1").join(frame_file(1)).exists());
            assert_eq!(
                fs::read_to_string(dir.join("run1").join(INDEX_FILE))
                    .expect("index")
                    .lines()
                    .count(),
                1
            );
        }
    }

    /// A pass the recorder runs in for another node's sake is not a frame.
    #[test]
    fn a_recorder_without_a_delivered_frame_writes_nothing() {
        let dir = temp_dir("untriggered");
        let dir_text = dir.to_string_lossy().into_owned();
        let mut writer = RecorderNode::new();
        set(&mut writer, "dir", &dir_text);
        set(&mut writer, "session", "run1");

        let mut inputs = InputSet::new();
        inputs.insert("frame", Value::new(frame(0x30)));
        let mut quiet = ctx();
        writer.execute(&inputs, &mut quiet).expect("no-op");
        assert_eq!(
            quiet
                .take_outputs()
                .get("count")
                .and_then(Value::downcast_ref::<i64>),
            Some(&0)
        );
        assert!(!dir.join("run1").exists(), "nothing was created");
    }

    /// The spare slot is what a new wire lands on, and there is exactly one:
    /// the host redraws on every `true`, so an unchanged pin set must say so.
    #[test]
    fn the_recorder_keeps_exactly_one_spare_value_input() {
        let mut writer = RecorderNode::new();
        let names = |node: &RecorderNode| -> Vec<String> {
            node.pin_definitions()
                .iter()
                .map(|p| p.name.to_string())
                .collect()
        };
        assert_eq!(
            names(&writer),
            vec!["frame", "v0", "count", "session", "path"]
        );

        let float = Ty::Float;
        let bound = |name: &'static str| PinBinding { name, ty: &float };
        assert!(writer.sync_pins(&[bound("v0")]));
        assert_eq!(
            names(&writer),
            vec!["frame", "v0", "v1", "count", "session", "path"]
        );
        // Same wiring again: nothing changed, so nothing is reported.
        assert!(!writer.sync_pins(&[bound("v0")]));
        // The frame pin is not a value slot.
        assert!(!writer.sync_pins(&[bound("v0"), bound("frame")]));
        assert!(writer.sync_pins(&[bound("v0"), bound("v1")]));
        assert_eq!(
            names(&writer),
            vec!["frame", "v0", "v1", "v2", "count", "session", "path"]
        );
        // Unwiring shrinks it back to one spare.
        assert!(writer.sync_pins(&[]));
        assert_eq!(
            names(&writer),
            vec!["frame", "v0", "count", "session", "path"]
        );
    }

    /// A player is driven by time, and the rate it asks for follows `hz`.
    #[test]
    fn the_player_asks_to_be_ticked_at_its_rate() {
        let mut player = PlayerNode::new();
        assert_eq!(player.tick_interval(), Some(Duration::from_millis(100)));
        set(&mut player, "hz", "5");
        assert_eq!(player.tick_interval(), Some(Duration::from_millis(200)));
        // Out of range is clamped, unparsable is ignored.
        set(&mut player, "hz", "9000");
        assert_eq!(
            player.tick_interval(),
            Some(Duration::from_secs_f64(1.0 / PlayerNode::MAX_HZ))
        );
        set(&mut player, "hz", "later");
        assert_eq!(
            player.tick_interval(),
            Some(Duration::from_secs_f64(1.0 / PlayerNode::MAX_HZ))
        );
    }

    /// A player with nothing to play says which of the two things is missing.
    #[test]
    fn a_player_without_a_recording_reports_it() {
        let mut player = PlayerNode::new();
        let unnamed = player
            .execute(&InputSet::new(), &mut ctx())
            .expect_err("no session");
        assert!(unnamed.to_string().contains("no session"));

        let dir = temp_dir("missing");
        set(&mut player, "dir", &dir.to_string_lossy());
        set(&mut player, "session", "nope");
        let missing = player
            .execute(&InputSet::new(), &mut ctx())
            .expect_err("no such recording");
        assert!(missing.to_string().contains(INDEX_FILE));
    }
}
