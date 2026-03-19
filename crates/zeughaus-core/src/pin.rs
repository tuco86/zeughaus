#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataMode {
    Stream,
    Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinKind {
    Trigger,
    Sample,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
