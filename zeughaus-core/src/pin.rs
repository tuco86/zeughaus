use serde::{Deserialize, Serialize};

/// What an output pin produces. Only `Value` is currently used.
/// `Stream` is reserved for high-frequency data (see DESIGN.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataMode {
    Stream,
    Value,
}

/// How an input pin consumes data. Not yet enforced by the executor.
/// `Trigger` causes node execution, `Sample` reads passively (see DESIGN.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PinKind {
    Trigger,
    Sample,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PinDirection {
    Input,
    Output,
}

#[derive(Debug, Clone)]
pub struct PinDefinition {
    pub name: &'static str,
    pub direction: PinDirection,
    pub data_mode: DataMode,
    pub pin_kind: PinKind,
    pub type_name: &'static str,
}
