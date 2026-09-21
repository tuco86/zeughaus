use crate::id::EdgeId;

/// Which of the wires contending for one single-slot input pin survives.
///
/// An input pin holds one edge, and a local connect cannot land on an
/// occupied one. Two editors can still each draw a wire onto the same input
/// without seeing the other's, so both rows reach the store and every client
/// has to reach the same verdict -- from the data alone, because arrival
/// order differs per client and deciding by it leaves the graph permanently
/// different in every window.
///
/// The largest [`EdgeId`] wins. Ids are unique across processes but carry no
/// time order, so which of the two wires that is, is arbitrary; that
/// everybody computes the same answer is the point. `None` for no contenders.
pub fn occupancy_winner(contenders: impl IntoIterator<Item = EdgeId>) -> Option<EdgeId> {
    contenders.into_iter().max()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every client must pick the same wire whatever order the rows reached
    /// it, so the verdict may depend on the ids and on nothing else.
    #[test]
    fn the_occupancy_winner_does_not_depend_on_order() {
        let ids = [EdgeId(7), EdgeId(42), EdgeId(13)];
        assert_eq!(occupancy_winner(ids), Some(EdgeId(42)));
        let mut reversed = ids;
        reversed.reverse();
        assert_eq!(occupancy_winner(reversed), Some(EdgeId(42)));
        assert_eq!(occupancy_winner([]), None);
    }
}
