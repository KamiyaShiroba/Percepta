//! How the panels are arranged inside a window: a tree of splits, whose leaves are stacks
//! of panels shown one at a time. Only the sizes here are the tree's business. Drawing it
//! is up to the app.

use serde::{Deserialize, Serialize};

/// All of these are at 100% scale.
const MIN_PANEL_WIDTH: f32 = 240.0;
/// The strip above a panel or a stack of tabs, with its name, that it is dragged by.
pub const MODULE_TITLE_HEIGHT: f32 = 20.0;
/// The colour slots, the wheel and the sliders, as the order that `Node::stack` puts them in.
/// All with their title strips.
pub const MIN_HEIGHTS: [f32; 3] = [80.0, 220.0, 170.0];
pub const DEFAULT_HEIGHTS: [f32; 3] = [92.0, 360.0, 180.0];

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Panel {
    Slots,
    Wheel,
    Sliders,
}

const PANELS: [Panel; 3] = [Panel::Slots, Panel::Wheel, Panel::Sliders];

impl Panel {
    fn min_size(self, axis: Axis) -> f32 {
        match (axis, self) {
            (Axis::Horizontal, _) => MIN_PANEL_WIDTH,
            (Axis::Vertical, Panel::Slots) => MIN_HEIGHTS[0],
            (Axis::Vertical, Panel::Wheel) => MIN_HEIGHTS[1],
            (Axis::Vertical, Panel::Sliders) => MIN_HEIGHTS[2],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// Where something dropped on a stack of tabs goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Drop {
    /// Among its tabs, before the one at this index, or after them all if there is none.
    Tab(usize),
    /// Beside it, before it along the axis or after it.
    Side { axis: Axis, before: bool },
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Child {
    pub node: Node,
    /// Along the axis of the split that holds it.
    pub size: f32,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum Node {
    Tabs { panels: Vec<Panel>, active: usize },
    Split { axis: Axis, children: Vec<Child> },
}

impl Default for Node {
    fn default() -> Self {
        Self::stack(DEFAULT_HEIGHTS)
    }
}

impl Node {
    /// The arrangement there has always been: the panels above one another.
    pub fn stack(heights: [f32; 3]) -> Self {
        let children = PANELS.iter().zip(heights).map(|(&panel, size)| Child {
            node: Node::Tabs { panels: vec![panel], active: 0 },
            size,
        });
        Node::Split { axis: Axis::Vertical, children: children.collect() }
    }

    /// Whether every panel appears exactly once, which a damaged file can't be trusted to do.
    pub fn is_complete(&self) -> bool {
        fn collect(node: &Node, found: &mut Vec<Panel>) -> bool {
            match node {
                Node::Tabs { panels, .. } => {
                    found.extend(panels);
                    !panels.is_empty()
                }
                Node::Split { children, .. } => {
                    !children.is_empty() && children.iter().all(|child| collect(&child.node, found))
                }
            }
        }
        let mut found = Vec::new();
        collect(self, &mut found)
            && found.len() == PANELS.len()
            && PANELS.iter().all(|panel| found.contains(panel))
    }

    pub fn tabs(panels: Vec<Panel>) -> Self {
        Node::Tabs { panels, active: 0 }
    }

    /// Every panel in the node.
    pub fn panels(&self) -> Vec<Panel> {
        match self {
            Node::Tabs { panels, .. } => panels.clone(),
            Node::Split { children, .. } => children.iter().flat_map(|child| child.node.panels()).collect(),
        }
    }

    /// Takes a panel out, closing up the room it leaves. The node must hold something else.
    pub fn remove(&mut self, panel: Panel) {
        match self {
            Node::Tabs { panels, active } => {
                if let Some(index) = panels.iter().position(|&other| other == panel) {
                    panels.remove(index);
                    if *active > index || *active >= panels.len() {
                        *active = active.saturating_sub(1);
                    }
                }
            }
            Node::Split { children, .. } => {
                for child in children.iter_mut() {
                    child.node.remove(panel);
                }
                let freed: f32 = children.iter().filter(|child| child.node.panels().is_empty()).map(|child| child.size).sum();
                children.retain(|child| !child.node.panels().is_empty());
                if let Some(last) = children.last_mut() {
                    last.size += freed;
                }
                if children.len() == 1 {
                    *self = children.remove(0).node;
                }
            }
        }
    }

    /// Puts `panels` where `drop` says, relative to the stack of tabs that holds `target`.
    pub fn dock(&mut self, target: Panel, drop: Drop, panels: Vec<Panel>) {
        if panels.is_empty() {
            return;
        }
        match drop {
            Drop::Tab(index) => {
                if let Some(Node::Tabs { panels: tabs, active }) = self.stack_mut(target) {
                    let index = index.min(tabs.len());
                    *active = index;
                    tabs.splice(index..index, panels);
                }
            }
            Drop::Side { axis, before } => self.dock_beside(target, axis, before, panels),
        }
    }

    fn dock_beside(&mut self, target: Panel, axis: Axis, before: bool, panels: Vec<Panel>) {
        match self {
            Node::Split { axis: along, children } => {
                let Some(index) = children.iter().position(|child| child.node.panels().contains(&target)) else {
                    return;
                };
                if *along == axis && matches!(children[index].node, Node::Tabs { .. }) {
                    let half = children[index].size / 2.0;
                    children[index].size = half;
                    let at = if before { index } else { index + 1 };
                    children.insert(at, Child { node: Node::tabs(panels), size: half });
                } else {
                    children[index].node.dock_beside(target, axis, before, panels);
                }
            }
            Node::Tabs { panels: tabs, .. } if tabs.contains(&target) => {
                // Sizes that the next fit will replace.
                let this = Child { node: std::mem::take(self), size: 1.0 };
                let other = Child { node: Node::tabs(panels), size: 1.0 };
                let children = if before { vec![other, this] } else { vec![this, other] };
                *self = Node::Split { axis, children };
            }
            Node::Tabs { .. } => {}
        }
    }

    fn stack_mut(&mut self, panel: Panel) -> Option<&mut Node> {
        match self {
            Node::Tabs { panels, .. } => panels.contains(&panel).then_some(self),
            Node::Split { children, .. } => children.iter_mut().find_map(|child| child.node.stack_mut(panel)),
        }
    }

    pub fn min_size(&self, axis: Axis) -> f32 {
        match self {
            Node::Tabs { panels, .. } => panels.iter().map(|panel| panel.min_size(axis)).fold(0.0, f32::max),
            Node::Split { axis: along, children } => {
                let sizes = children.iter().map(|child| child.node.min_size(axis));
                if *along == axis { sizes.sum() } else { sizes.fold(0.0, f32::max) }
            }
        }
    }

    /// Makes everything in the node fill `width` by `height`. In each split, spare room goes
    /// to the last child, and a shortage comes from the last child first, as far as the
    /// minimum sizes allow.
    pub fn fit(&mut self, width: f32, height: f32) {
        let Node::Split { axis, children } = self else { return };
        let (total, across) = match axis {
            Axis::Horizontal => (width, height),
            Axis::Vertical => (height, width),
        };
        let axis = *axis;
        let mins: Vec<f32> = children.iter().map(|child| child.node.min_size(axis)).collect();
        for (child, min) in children.iter_mut().zip(&mins) {
            child.size = child.size.max(*min);
        }
        let mut spare = total - children.iter().map(|child| child.size).sum::<f32>();
        if spare >= 0.0 {
            if let Some(last) = children.last_mut() {
                last.size += spare;
            }
        } else {
            for (child, min) in children.iter_mut().zip(&mins).rev() {
                let taken = (child.size - min).min(-spare);
                child.size -= taken;
                spare += taken;
            }
        }
        for child in children {
            match axis {
                Axis::Horizontal => child.node.fit(child.size, across),
                Axis::Vertical => child.node.fit(across, child.size),
            }
        }
    }

    /// The same node with whole sizes, which is all that is worth saving.
    pub fn rounded(&self) -> Self {
        let mut node = self.clone();
        if let Node::Split { children, .. } = &mut node {
            for child in children {
                child.size = child.size.round();
                child.node = child.node.rounded();
            }
        }
        node
    }

    /// The length of this node along `axis`, if its own children decide that.
    pub fn total(&self, axis: Axis) -> Option<f32> {
        match self {
            Node::Split { axis: along, children } if *along == axis => {
                Some(children.iter().map(|child| child.size).sum())
            }
            _ => None,
        }
    }
}

/// Applies dragging the line after `children[line]` by `delta`, along `axis`, and returns by
/// how much the window should grow. A child that grows makes the window bigger, while one
/// that shrinks hands its room to the last child and leaves the window as it is.
pub fn drag_line(children: &mut [Child], axis: Axis, line: usize, delta: f32) -> f32 {
    let min = children[line].node.min_size(axis);
    let new_size = (children[line].size + delta).max(min);
    let change = new_size - children[line].size;
    children[line].size = new_size;
    if change >= 0.0 {
        return change;
    }
    if let Some(last) = children.last_mut() {
        last.size -= change;
    }
    0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heights(node: &Node) -> Vec<f32> {
        match node {
            Node::Split { children, .. } => children.iter().map(|child| child.size).collect(),
            Node::Tabs { .. } => Vec::new(),
        }
    }

    fn fitted(start: [f32; 3], total: f32) -> Vec<f32> {
        let mut node = Node::stack(start);
        node.fit(380.0, total);
        heights(&node)
    }

    #[test]
    fn panels_fill_the_window_and_only_give_way_down_to_their_minimums() {
        assert_eq!(fitted([92.0, 360.0, 228.0], 780.0), [92.0, 360.0, 328.0]);
        assert_eq!(fitted([92.0, 360.0, 228.0], 590.0), [92.0, 328.0, 170.0]);
        // The last panel gives way first, then the one above it.
        assert_eq!(fitted([92.0, 360.0, 228.0], 530.0), [92.0, 268.0, 170.0]);
        assert_eq!(fitted([92.0, 360.0, 228.0], 470.0), [80.0, 220.0, 170.0]);
        // A saved height below the minimum is raised.
        assert_eq!(fitted([10.0, 220.0, 170.0], 470.0), [80.0, 220.0, 170.0]);
    }

    fn dragged(line: usize, delta: f32) -> (Vec<f32>, f32) {
        let mut node = Node::stack([92.0, 360.0, 228.0]);
        let Node::Split { children, .. } = &mut node else { unreachable!() };
        let grow = drag_line(children, Axis::Vertical, line, delta);
        (heights(&node), grow)
    }

    #[test]
    fn growing_a_panel_grows_the_window_but_shrinking_one_does_not() {
        assert_eq!(dragged(1, 30.0), (vec![92.0, 390.0, 228.0], 30.0));
        assert_eq!(dragged(1, -30.0), (vec![92.0, 330.0, 258.0], 0.0));
        // Stops at the minimum, and the rest of the drag does nothing.
        assert_eq!(dragged(0, -30.0), (vec![80.0, 360.0, 240.0], 0.0));
    }

    #[test]
    fn removing_a_panel_gives_its_room_away_and_collapses_what_is_left() {
        let mut node = Node::stack([92.0, 360.0, 228.0]);
        node.remove(Panel::Wheel);
        assert_eq!(node.panels(), [Panel::Slots, Panel::Sliders]);
        assert_eq!(heights(&node), [92.0, 588.0]);
        node.remove(Panel::Slots);
        assert_eq!(node, Node::tabs(vec![Panel::Sliders]));
    }

    #[test]
    fn removing_a_tab_keeps_the_stack_and_a_valid_active_tab() {
        let mut node = Node::Tabs { panels: PANELS.to_vec(), active: 2 };
        node.remove(Panel::Sliders);
        assert_eq!(node, Node::Tabs { panels: vec![Panel::Slots, Panel::Wheel], active: 1 });
        node.remove(Panel::Slots);
        assert_eq!(node, Node::Tabs { panels: vec![Panel::Wheel], active: 0 });
    }

    #[test]
    fn a_panel_can_land_as_a_tab_or_on_either_side() {
        let mut node = Node::stack([92.0, 360.0, 228.0]);
        node.remove(Panel::Wheel);
        let mut tabbed = node.clone();
        tabbed.dock(Panel::Slots, Drop::Tab(usize::MAX), vec![Panel::Wheel]);
        assert_eq!(tabbed.panels(), [Panel::Slots, Panel::Wheel, Panel::Sliders]);
        assert!(tabbed.is_complete());
        assert_eq!(heights(&tabbed), [92.0, 588.0]);

        let mut above = node.clone();
        let side = |before| Drop::Side { axis: Axis::Vertical, before };
        above.dock(Panel::Sliders, side(true), vec![Panel::Wheel]);
        assert_eq!(above.panels(), [Panel::Slots, Panel::Wheel, Panel::Sliders]);
        assert_eq!(heights(&above), [92.0, 294.0, 294.0]);

        let mut below = node;
        below.dock(Panel::Slots, side(false), vec![Panel::Wheel]);
        assert_eq!(below.panels(), [Panel::Slots, Panel::Wheel, Panel::Sliders]);
        assert_eq!(heights(&below), [46.0, 46.0, 588.0]);
    }

    #[test]
    fn a_tab_lands_where_it_was_aimed_and_becomes_the_active_one() {
        let mut node = Node::tabs(vec![Panel::Slots, Panel::Sliders]);
        node.dock(Panel::Slots, Drop::Tab(1), vec![Panel::Wheel]);
        assert_eq!(node, Node::Tabs { panels: PANELS.to_vec(), active: 1 });

        let mut first = Node::tabs(vec![Panel::Wheel]);
        first.dock(Panel::Wheel, Drop::Tab(0), vec![Panel::Slots]);
        assert_eq!(first, Node::Tabs { panels: vec![Panel::Slots, Panel::Wheel], active: 0 });
    }

    #[test]
    fn docking_on_a_lone_stack_or_across_a_split_makes_a_new_split() {
        let mut lone = Node::tabs(vec![Panel::Wheel]);
        lone.dock(Panel::Wheel, Drop::Side { axis: Axis::Vertical, before: true }, vec![Panel::Slots]);
        assert_eq!(lone.panels(), [Panel::Slots, Panel::Wheel]);

        let mut node = Node::stack([92.0, 360.0, 228.0]);
        node.remove(Panel::Sliders);
        node.dock(Panel::Wheel, Drop::Side { axis: Axis::Horizontal, before: false }, vec![Panel::Sliders]);
        assert_eq!(node.panels(), [Panel::Slots, Panel::Wheel, Panel::Sliders]);
        let Node::Split { axis: Axis::Vertical, children } = &node else { unreachable!() };
        assert!(matches!(children[1].node, Node::Split { axis: Axis::Horizontal, .. }));
    }

    /// The wheel beside the slots and sliders, which sit above one another.
    fn nested() -> Node {
        let tabs = |panel| Node::Tabs { panels: vec![panel], active: 0 };
        Node::Split {
            axis: Axis::Horizontal,
            children: vec![
                Child { node: tabs(Panel::Wheel), size: 300.0 },
                Child {
                    node: Node::Split {
                        axis: Axis::Vertical,
                        children: vec![
                            Child { node: tabs(Panel::Slots), size: 100.0 },
                            Child { node: tabs(Panel::Sliders), size: 200.0 },
                        ],
                    },
                    size: 300.0,
                },
            ],
        }
    }

    #[test]
    fn a_nested_split_fits_itself_and_counts_towards_its_parents_minimums() {
        let mut node = nested();
        assert_eq!(node.min_size(Axis::Horizontal), 480.0);
        // The taller of the wheel, and the slots and sliders together.
        assert_eq!(node.min_size(Axis::Vertical), 250.0);
        node.fit(800.0, 500.0);
        assert_eq!(heights(&node), [300.0, 500.0]);
        let Node::Split { children, .. } = &node else { unreachable!() };
        assert_eq!(heights(&children[1].node), [100.0, 400.0]);
    }

    #[test]
    fn only_a_split_along_the_axis_decides_its_total() {
        let node = nested();
        assert_eq!(node.total(Axis::Horizontal), Some(600.0));
        assert_eq!(node.total(Axis::Vertical), None);
    }

    #[test]
    fn a_tree_must_hold_every_panel_exactly_once() {
        assert!(Node::default().is_complete());
        assert!(nested().is_complete());
        let tabs = |panels: &[Panel]| Node::Tabs { panels: panels.to_vec(), active: 0 };
        assert!(tabs(&PANELS).is_complete());
        assert!(!tabs(&[Panel::Wheel, Panel::Wheel, Panel::Slots]).is_complete());
        assert!(!tabs(&[Panel::Wheel]).is_complete());
        assert!(!Node::Split { axis: Axis::Vertical, children: Vec::new() }.is_complete());
    }

    #[test]
    fn rounding_reaches_nested_sizes() {
        let mut node = nested();
        node.fit(800.4, 500.4);
        let Node::Split { children, .. } = node.rounded() else { unreachable!() };
        assert_eq!(children[1].size, 500.0);
        assert_eq!(heights(&children[1].node), [100.0, 400.0]);
    }
}
