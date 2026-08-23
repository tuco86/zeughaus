//! Flow plugin: runtime-flow primitives that bridge the two transmission modes
//! of the dataflow -- Events (one-shot, delivered on Trigger pins) and State
//! (last-value, sampled on Sample pins).
//!
//! `Hold` is the explicit Event -> State adapter: it latches the most recent
//! event value of any type and exposes it as a steady, samplable state. Its
//! pin kinds (Trigger in, Sample out) declare the conversion; once the executor
//! enforces the Trigger/Sample distinction, this node is where a transient
//! event stream becomes a persistent value other nodes can sample.
//!
//! `Button` is the opposite end of the same bridge: it is the origin of an
//! Event. Nothing upstream produces it -- a human press does -- which is why it
//! belongs here next to `Hold` rather than in a domain plugin.

use zeughaus_core::*;

pub struct FlowPlugin;

impl DomainPlugin for FlowPlugin {
    fn name(&self) -> &str {
        "flow"
    }

    fn node_catalog(&self) -> Vec<NodeDefinition> {
        vec![
            catalog_entry("flow.hold", "Hold", "Flow", &HoldNode::new()),
            catalog_entry("flow.button", "Button", "Flow", &ButtonNode::new()),
            catalog_entry("flow.timer", "Timer", "Flow", &TimerNode::new()),
        ]
    }

    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>> {
        match type_id {
            "flow.hold" => Some(Box::new(HoldNode::new())),
            "flow.button" => Some(Box::new(ButtonNode::new())),
            "flow.timer" => Some(Box::new(TimerNode::new())),
            _ => None,
        }
    }
}

/// Sample & Hold: turns an Event of any type into State by latching the most
/// recent value. Between events it keeps emitting the last one, so downstream
/// nodes sample a steady value rather than a momentary pulse.
pub struct HoldNode {
    last: Option<Value>,
    pins: Vec<PinDefinition>,
}

impl Default for HoldNode {
    fn default() -> Self {
        Self::new()
    }
}

impl HoldNode {
    pub fn new() -> Self {
        Self {
            last: None,
            pins: vec![
                PinDefinition::input("in", Ty::Any, PinKind::Trigger),
                PinDefinition::output("out", Ty::Any),
            ],
        }
    }
}

impl ExecutableNode for HoldNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        // Latch a new event; otherwise keep the previously held value.
        if let Some(v) = inputs.get_value("in") {
            self.last = Some(v.clone());
        }
        // Emit the held state. Before the first event there is nothing to hold.
        if let Some(v) = &self.last {
            ctx.emit("out", v.clone());
            ctx.flush();
        }
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}

/// Manual trigger: the origin of an Event, and the counterpart to `Hold` (which
/// turns Events into State). A press is the whole signal, so there is nothing to
/// compute and nothing upstream to read.
///
/// Pressing arms the node via the parameter channel; the next execute emits a
/// single `out = true` and disarms. An unarmed execute emits *nothing* rather
/// than `false`: an event that did not happen has no value, so the pin reads as
/// absent between presses and downstream Trigger inputs stay quiet.
pub struct ButtonNode {
    armed: bool,
    pins: Vec<PinDefinition>,
}

impl Default for ButtonNode {
    fn default() -> Self {
        Self::new()
    }
}

impl ButtonNode {
    pub fn new() -> Self {
        Self {
            armed: false,
            pins: vec![PinDefinition::output("out", Ty::Bool)],
        }
    }
}

impl ExecutableNode for ButtonNode {
    fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        // One press, one event: consume the arming so a later execute without a
        // new press stays silent.
        if std::mem::take(&mut self.armed) {
            ctx.emit("out", Value::new(true));
            ctx.flush();
        }
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    /// The press itself is the signal, so the value carried by `fire` is
    /// irrelevant -- any value arms the node.
    fn set_parameter(&mut self, name: &str, _value: Value) -> Result<()> {
        if name == "fire" {
            self.armed = true;
        }
        Ok(())
    }
}

/// A clock: fires `tick` every `hz`-th of a second, driven by nothing but time.
///
/// This is what makes a source node a *source*. A screen capture is not woken by
/// data arriving upstream, so without a clock the graph produces one frame and
/// then sits still -- nothing ever marks the node dirty again. The timer states
/// its interval through [`ExecutableNode::tick_interval`] and the host does the
/// scheduling; a node that slept would block the pass it runs in.
pub struct TimerNode {
    hz: f64,
    pins: Vec<PinDefinition>,
}

impl Default for TimerNode {
    fn default() -> Self {
        Self::new()
    }
}

impl TimerNode {
    /// 30 Hz by default: enough for a live preview, and half the work of 60.
    pub const DEFAULT_HZ: f64 = 30.0;
    /// Ceilings on the configured rate. Zero would mean "never" more clearly
    /// expressed by deleting the node, and a runaway value would spin the host.
    pub const MIN_HZ: f64 = 0.01;
    pub const MAX_HZ: f64 = 240.0;

    pub fn new() -> Self {
        Self {
            hz: Self::DEFAULT_HZ,
            pins: vec![PinDefinition::output("tick", Ty::Bool)],
        }
    }

    pub fn hz(&self) -> f64 {
        self.hz
    }
}

impl ExecutableNode for TimerNode {
    fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        // Every execution IS the tick: the host only runs this node when its
        // interval elapsed, so there is no schedule to re-check here.
        ctx.emit("tick", Value::new(true));
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![SettingDef {
            name: "hz".into(),
            default: Self::DEFAULT_HZ.to_string().into(),
            placeholder: "30".into(),
            multiline: false,
        }]
    }

    /// An unparsable or out-of-range rate keeps the previous one: a half-typed
    /// number in the editor must not stop the clock.
    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        if name != "hz" {
            return Ok(());
        }
        let parsed = match value.repr() {
            Repr::Float(f) => Some(f),
            Repr::Int(i) => Some(i as f64),
            Repr::Str(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        };
        if let Some(hz) = parsed.filter(|hz| hz.is_finite()) {
            self.hz = hz.clamp(Self::MIN_HZ, Self::MAX_HZ);
        }
        Ok(())
    }

    fn tick_interval(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_secs_f64(1.0 / self.hz))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_all_catalog_nodes() {
        let plugin = FlowPlugin;
        for def in plugin.node_catalog() {
            assert!(
                plugin.create_node(&def.type_id).is_some(),
                "failed: {}",
                def.type_id
            );
        }
    }

    #[test]
    fn latches_then_holds_last_value() {
        let mut node = HoldNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("in", Value::new(5.0f64));
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        assert_eq!(ctx.take_outputs()["out"].downcast_ref::<f64>(), Some(&5.0));

        // No event this run: the held state persists (event consumed, state stays).
        let mut ctx2 = NodeContext::new(NodeId(1), 0);
        node.execute(&InputSet::new(), &mut ctx2).unwrap();
        assert_eq!(ctx2.take_outputs()["out"].downcast_ref::<f64>(), Some(&5.0));
    }

    #[test]
    fn holds_any_type() {
        let mut node = HoldNode::new();
        let mut inputs = InputSet::new();
        inputs.insert("in", Value::new("hi".to_string()));
        let mut ctx = NodeContext::new(NodeId(2), 0);
        node.execute(&inputs, &mut ctx).unwrap();
        let out = ctx.take_outputs();
        assert_eq!(
            out["out"].downcast_ref::<String>().map(String::as_str),
            Some("hi")
        );
    }

    #[test]
    fn no_output_before_first_event() {
        let mut node = HoldNode::new();
        let mut ctx = NodeContext::new(NodeId(3), 0);
        node.execute(&InputSet::new(), &mut ctx).unwrap();
        assert!(ctx.take_outputs().is_empty());
    }

    #[test]
    fn button_emits_nothing_until_pressed() {
        let mut node = ButtonNode::new();
        let mut ctx = NodeContext::new(NodeId(4), 0);
        node.execute(&InputSet::new(), &mut ctx).unwrap();
        assert!(ctx.take_outputs().is_empty());
    }

    #[test]
    fn button_fires_once_per_press() {
        let mut node = ButtonNode::new();
        node.set_parameter("fire", Value::new(true)).unwrap();

        let mut ctx = NodeContext::new(NodeId(5), 0);
        node.execute(&InputSet::new(), &mut ctx).unwrap();
        assert_eq!(
            ctx.take_outputs()["out"].downcast_ref::<bool>(),
            Some(&true)
        );

        // Not re-armed: the event was consumed, so nothing is emitted.
        let mut ctx2 = NodeContext::new(NodeId(5), 0);
        node.execute(&InputSet::new(), &mut ctx2).unwrap();
        assert!(ctx2.take_outputs().is_empty());
    }

    #[test]
    fn button_arms_regardless_of_parameter_value() {
        let mut node = ButtonNode::new();
        node.set_parameter("fire", Value::new(false)).unwrap();
        let mut ctx = NodeContext::new(NodeId(6), 0);
        node.execute(&InputSet::new(), &mut ctx).unwrap();
        assert_eq!(
            ctx.take_outputs()["out"].downcast_ref::<bool>(),
            Some(&true)
        );
    }

    #[test]
    fn button_catalog_entry_has_single_out_pin() {
        let plugin = FlowPlugin;
        let def = plugin
            .node_catalog()
            .into_iter()
            .find(|d| &*d.type_id == "flow.button")
            .expect("flow.button missing from catalog");
        assert_eq!(&*def.display_name, "Button");
        assert_eq!(&*def.category, "Flow");

        let node = plugin
            .create_node("flow.button")
            .expect("flow.button not creatable");
        let pins = node.pin_definitions();
        assert_eq!(pins.len(), 1);
        assert_eq!(&*pins[0].name, "out");
        assert_eq!(pins[0].direction, PinDirection::Output);
        assert_eq!(pins[0].ty, Ty::Bool);
    }

    #[test]
    fn a_timer_ticks_every_execution() {
        let mut node = TimerNode::new();
        let mut ctx = NodeContext::new(NodeId(1), 0);
        node.execute(&InputSet::new(), &mut ctx).unwrap();
        assert_eq!(
            ctx.take_outputs()["tick"].downcast_ref::<bool>(),
            Some(&true)
        );
    }

    #[test]
    fn the_default_rate_is_a_preview_rate() {
        let node = TimerNode::new();
        assert_eq!(node.hz(), 30.0);
        assert_eq!(
            node.tick_interval(),
            Some(std::time::Duration::from_secs_f64(1.0 / 30.0))
        );
    }

    #[test]
    fn the_rate_comes_from_the_hz_setting() {
        let mut node = TimerNode::new();
        node.set_parameter("hz", Value::new("5".to_string())).unwrap();
        assert_eq!(node.hz(), 5.0);
        node.set_parameter("hz", Value::new(12.0f64)).unwrap();
        assert_eq!(node.hz(), 12.0);
    }

    /// A half-typed number in the editor must not stop the clock, and a runaway
    /// value must not spin the host.
    #[test]
    fn a_nonsense_rate_is_refused_or_clamped() {
        let mut node = TimerNode::new();
        node.set_parameter("hz", Value::new("".to_string())).unwrap();
        assert_eq!(node.hz(), TimerNode::DEFAULT_HZ);
        node.set_parameter("hz", Value::new("-3".to_string())).unwrap();
        assert_eq!(node.hz(), TimerNode::MIN_HZ);
        node.set_parameter("hz", Value::new(100_000.0f64)).unwrap();
        assert_eq!(node.hz(), TimerNode::MAX_HZ);
    }

    /// Only the timer asks to be run on a clock; every other flow node waits for
    /// data.
    #[test]
    fn only_the_timer_wants_a_clock() {
        assert!(HoldNode::new().tick_interval().is_none());
        assert!(ButtonNode::new().tick_interval().is_none());
        assert!(TimerNode::new().tick_interval().is_some());
    }
}
