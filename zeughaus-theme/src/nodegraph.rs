//! The node graph's catalog.
//!
//! Every class is the widget's own boxed closure over this theme, and every
//! default resolves the graph crate's built-in style against the inner iced
//! theme. The editor overrides the few that carry meaning -- a pin's type, a
//! node's category -- by starting from the same defaults.

use iced_nodegraph::{
    AnchorStatus, AnchorStyle, AnchorStyleFn, Catalog, CuttingToolStyle, CuttingToolStyleFn,
    DragEdgeStyleFn, EdgeStatus, EdgeStyle, EdgeStyleFn, GraphStyle, GraphStyleFn, Ids,
    MinimapStyle, MinimapStyleFn, NodeStatus, NodeStyle, NodeStyleFn, ParticleStyle,
    ParticleStyleFn, PinInfo, PinStatus, PinStyle, PinStyleFn, SelectionBoxStyle,
    SelectionBoxStyleFn, default_anchor_style, default_cutting_tool_style, default_edge_style,
    default_graph_style, default_minimap_style, default_node_style, default_particle_style,
    default_pin_style, default_selection_box_style,
};

use crate::Theme;

impl Catalog for Theme {
    type NodeClass<'a> = NodeStyleFn<'a, Self>;
    type PinClass<'a, I: Ids> = PinStyleFn<'a, Self, I>;
    type EdgeClass<'a, I: Ids> = EdgeStyleFn<'a, Self, I>;
    type DragEdgeClass<'a, I: Ids> = DragEdgeStyleFn<'a, Self, I>;
    type AnchorClass<'a> = AnchorStyleFn<'a, Self>;
    type GraphClass<'a> = GraphStyleFn<'a, Self>;
    type SelectionBoxClass<'a> = SelectionBoxStyleFn<'a, Self>;
    type CuttingToolClass<'a> = CuttingToolStyleFn<'a, Self>;
    type ParticleClass<'a> = ParticleStyleFn<'a, Self>;
    type MinimapClass<'a> = MinimapStyleFn<'a, Self>;

    fn default_node<'a>() -> Self::NodeClass<'a> {
        Box::new(|theme: &Self, status| default_node_style(theme.base(), status))
    }

    fn node(&self, class: &Self::NodeClass<'_>, status: NodeStatus) -> NodeStyle {
        class(self, status)
    }

    fn default_pin<'a, I: Ids>() -> Self::PinClass<'a, I> {
        Box::new(|theme: &Self, _pin, _other, status| default_pin_style(theme.base(), status))
    }

    fn pin<I: Ids>(
        &self,
        class: &Self::PinClass<'_, I>,
        pin: &PinInfo<'_, I>,
        other: Option<&PinInfo<'_, I>>,
        status: PinStatus,
    ) -> PinStyle {
        class(self, pin, other, status)
    }

    fn default_edge<'a, I: Ids>() -> Self::EdgeClass<'a, I> {
        Box::new(|theme: &Self, status, _from, _to| default_edge_style(theme.base(), status))
    }

    fn edge<I: Ids>(
        &self,
        class: &Self::EdgeClass<'_, I>,
        status: EdgeStatus,
        from: PinInfo<'_, I>,
        to: PinInfo<'_, I>,
    ) -> EdgeStyle {
        class(self, status, from, to)
    }

    fn default_drag_edge<'a, I: Ids>() -> Self::DragEdgeClass<'a, I> {
        Box::new(|theme: &Self, _source| default_edge_style(theme.base(), EdgeStatus::Idle))
    }

    fn drag_edge<I: Ids>(
        &self,
        class: &Self::DragEdgeClass<'_, I>,
        source: PinInfo<'_, I>,
    ) -> EdgeStyle {
        class(self, source)
    }

    fn default_anchor<'a>() -> Self::AnchorClass<'a> {
        Box::new(|theme: &Self, status| default_anchor_style(theme.base(), status))
    }

    fn anchor(&self, class: &Self::AnchorClass<'_>, status: AnchorStatus) -> AnchorStyle {
        class(self, status)
    }

    fn default_graph<'a>() -> Self::GraphClass<'a> {
        Box::new(|theme: &Self| default_graph_style(theme.base()))
    }

    fn graph(&self, class: &Self::GraphClass<'_>) -> GraphStyle {
        class(self)
    }

    fn default_selection_box<'a>() -> Self::SelectionBoxClass<'a> {
        Box::new(|theme: &Self| default_selection_box_style(theme.base()))
    }

    fn selection_box(&self, class: &Self::SelectionBoxClass<'_>) -> SelectionBoxStyle {
        class(self)
    }

    fn default_cutting_tool<'a>() -> Self::CuttingToolClass<'a> {
        Box::new(|theme: &Self| default_cutting_tool_style(theme.base()))
    }

    fn cutting_tool(&self, class: &Self::CuttingToolClass<'_>) -> CuttingToolStyle {
        class(self)
    }

    fn default_particle<'a>() -> Self::ParticleClass<'a> {
        Box::new(|theme: &Self| default_particle_style(theme.base()))
    }

    fn particle(&self, class: &Self::ParticleClass<'_>) -> ParticleStyle {
        class(self)
    }

    fn default_minimap<'a>() -> Self::MinimapClass<'a> {
        Box::new(|theme: &Self| default_minimap_style(theme.base()))
    }

    fn minimap(&self, class: &Self::MinimapClass<'_>) -> MinimapStyle {
        class(self)
    }
}
