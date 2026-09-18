use iced::Color;
use iced::widget::pane_grid::{self, Axis, Pane, ResizeEvent};
use iced_tabs::Placement;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Graph,
    Empty,
}

impl Surface {
    pub fn title(self) -> &'static str {
        match self {
            Self::Graph => "Graph",
            Self::Empty => "Empty",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Tab {
    pub id: TabId,
    pub title: String,
    pub group: String,
    pub accent: Color,
    pub panes: pane_grid::State<Surface>,
    pub active_pane: Pane,
}

impl Tab {
    fn graph(id: TabId, number: u64) -> Self {
        let (panes, active_pane) = pane_grid::State::new(Surface::Graph);
        Self {
            id,
            title: format!("Graph {number}"),
            group: "Graphs".to_owned(),
            accent: Color::from_rgb(0.28, 0.55, 0.92),
            panes,
            active_pane,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub tabs: Vec<Tab>,
    pub active_tab: TabId,
    pub placement: Placement,
    next_tab: u64,
}

impl Workspace {
    pub fn new() -> Self {
        let first = TabId(1);
        Self {
            tabs: vec![Tab::graph(first, 1)],
            active_tab: first,
            placement: Placement::Top,
            next_tab: 2,
        }
    }

    pub fn active(&self) -> &Tab {
        self.tabs
            .iter()
            .find(|tab| tab.id == self.active_tab)
            .expect("a workspace always has an active tab")
    }

    pub fn update(&mut self, message: Message) {
        match message {
            Message::ActivateTab(id) => {
                if self.tabs.iter().any(|tab| tab.id == id) {
                    self.active_tab = id;
                }
            }
            Message::CloseTab(id) => {
                if self.tabs.len() == 1 {
                    return;
                }
                let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
                    return;
                };
                self.tabs.remove(index);
                if self.active_tab == id {
                    self.active_tab = self.tabs[index.min(self.tabs.len() - 1)].id;
                }
            }
            Message::NewTab => {
                let number = self.next_tab;
                let id = TabId(number);
                self.next_tab += 1;
                self.tabs.push(Tab::graph(id, number));
                self.active_tab = id;
            }
            Message::TogglePlacement => {
                self.placement = match self.placement {
                    Placement::Top => Placement::Left,
                    Placement::Left => Placement::Top,
                };
            }
            Message::ActivatePane(pane) => {
                let tab = self.active_mut();
                if tab.panes.get(pane).is_some() {
                    tab.active_pane = pane;
                }
            }
            Message::SplitPane { pane, axis } => {
                let tab = self.active_mut();
                if let Some((new_pane, _)) = tab.panes.split(axis, pane, Surface::Empty) {
                    tab.active_pane = new_pane;
                }
            }
            Message::ClosePane(pane) => {
                let tab = self.active_mut();
                if tab.panes.iter().count() == 1 {
                    return;
                }
                let closing_graph = tab.panes.get(pane) == Some(&Surface::Graph);
                if let Some((_, sibling)) = tab.panes.close(pane) {
                    if closing_graph && let Some(surface) = tab.panes.get_mut(sibling) {
                        *surface = Surface::Graph;
                    }
                    tab.active_pane = sibling;
                }
            }
            Message::ResizePane(event) => {
                self.active_mut().panes.resize(event.split, event.ratio);
            }
        }
    }

    fn active_mut(&mut self) -> &mut Tab {
        self.tabs
            .iter_mut()
            .find(|tab| tab.id == self.active_tab)
            .expect("a workspace always has an active tab")
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    ActivateTab(TabId),
    CloseTab(TabId),
    NewTab,
    TogglePlacement,
    ActivatePane(Pane),
    SplitPane { pane: Pane, axis: Axis },
    ClosePane(Pane),
    ResizePane(ResizeEvent),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_tab_cannot_be_closed() {
        let mut workspace = Workspace::new();
        let only = workspace.active_tab;

        workspace.update(Message::CloseTab(only));

        assert_eq!(workspace.tabs.len(), 1);
        assert_eq!(workspace.active_tab, only);
    }

    #[test]
    fn closing_the_graph_pane_keeps_a_graph_surface() {
        let mut workspace = Workspace::new();
        let graph = workspace.active().active_pane;
        workspace.update(Message::SplitPane {
            pane: graph,
            axis: Axis::Vertical,
        });

        workspace.update(Message::ClosePane(graph));

        let tab = workspace.active();
        assert_eq!(tab.panes.iter().count(), 1);
        assert!(
            tab.panes
                .iter()
                .any(|(_, surface)| *surface == Surface::Graph)
        );
    }

    #[test]
    fn closing_the_active_tab_selects_a_neighbor() {
        let mut workspace = Workspace::new();
        workspace.update(Message::NewTab);
        let second = workspace.active_tab;

        workspace.update(Message::CloseTab(second));

        assert_eq!(workspace.tabs.len(), 1);
        assert_ne!(workspace.active_tab, second);
    }
}
