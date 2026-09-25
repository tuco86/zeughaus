//! What a trigger hands the node it fires.

use crate::ty::{Ty, Typed};

/// One press of a node, as the value of its `fire` parameter.
///
/// `payload` is the free text the pressing side attached (a webhook body, a
/// branch name; empty for a bare press). `external` says the press came from
/// outside an editor, which is what a job needs to know to show its run where
/// the user looks for runs nobody started by hand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Press {
    pub payload: String,
    pub external: bool,
}

impl Typed for Press {
    fn ty() -> Ty {
        Ty::opaque("press")
    }
}
