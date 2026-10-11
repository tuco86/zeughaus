//! The cables of a pipeline's job graph: one SDF primitive under the job
//! cards, drawn with the renderer `iced_nodegraph` draws its graphs with.
//!
//! The widget is a fixed-size leaf that takes no input, so the scrollable
//! around the channel keeps the wheel and the cards on top keep the pointer.

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{Tree, Widget};
use iced::{Color, Element, Length, Rectangle, Size, mouse};
use iced_nodegraph_sdf::{Pattern, SdfPrimitive, Shape, Style};

/// Stroke width of a cable, in logical pixels.
const CABLE_WIDTH: f32 = 2.0;
/// Radius of the dot where a cable meets a card's edge.
const PORT_RADIUS: f32 = 4.0;

/// A cable from `from` to `to`, both in the widget's own coordinates: it
/// leaves and arrives horizontally, like the cables of the node graph.
pub(super) fn cable(from: [f32; 2], to: [f32; 2]) -> Shape {
    let reach = ((to[0] - from[0]).abs() * 0.5).clamp(16.0, 80.0);
    Shape::bezier(from, [from[0] + reach, from[1]], [to[0] - reach, to[1]], to)
}

/// The cables and port dots of one pipeline, `size` large.
pub(super) struct Cables<'a> {
    size: Size,
    cables: &'a [Shape],
    ports: &'a [[f32; 2]],
    color: Color,
}

impl<'a> Cables<'a> {
    pub(super) fn new(
        size: Size,
        cables: &'a [Shape],
        ports: &'a [[f32; 2]],
        color: Color,
    ) -> Self {
        Cables {
            size,
            cables,
            ports,
            color,
        }
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for Cables<'_>
where
    Renderer: iced_wgpu::primitive::Renderer,
{
    fn size(&self) -> Size<Length> {
        Size::new(
            Length::Fixed(self.size.width),
            Length::Fixed(self.size.height),
        )
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        _limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(self.size)
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        // A channel holds many pipelines; only the ones in view cost a draw.
        if self.cables.is_empty() || !bounds.intersects(viewport) {
            return;
        }
        let stroke = Style::stroke(self.color, Pattern::solid(CABLE_WIDTH));
        let fill = Style::solid(self.color);
        let port = Shape::circle(PORT_RADIUS);
        let mut primitive = SdfPrimitive::with_capacity(self.cables.len() + self.ports.len());
        for cable in self.cables {
            primitive.push(cable, &stroke, [0.0, 0.0]);
        }
        for &at in self.ports {
            primitive.push(&port, &fill, at);
        }
        // World units are the widget's own pixels: no pan, no zoom.
        renderer.draw_primitive(
            bounds,
            primitive.camera(0.0, 0.0, 1.0).layout_bounds(bounds),
        );
    }
}

impl<'a, Message, Theme, Renderer> From<Cables<'a>> for Element<'a, Message, Theme, Renderer>
where
    Renderer: iced_wgpu::primitive::Renderer + 'a,
{
    fn from(cables: Cables<'a>) -> Self {
        Element::new(cables)
    }
}
