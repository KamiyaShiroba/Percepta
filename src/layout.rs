//! How the panels are arranged inside a window: a tree of splits, whose leaves are stacks
//! of panels shown one at a time. Only the sizes here are the tree's business. Drawing it
//! is up to the app.

use serde::{Deserialize, Serialize};

/// All of these are at 100% scale.
///
/// The colour slots, the wheel and the sliders, as the order that `Node::stack` puts them in.
pub const MIN_WIDTHS: [f32; 3] = [120.0, 170.0, 240.0];
/// The strip above a panel or a stack of tabs, with its name, that it is dragged by.
pub const MODULE_TITLE_HEIGHT: f32 = 20.0;
/// The colour slots stay a row below this height.
pub const SLOTS_SQUARE_MIN_HEIGHT: f32 = 200.0;
/// When they are alone in a window, and from `SLOTS_SQUARE_MIN_HEIGHT` on.
pub const SLOTS_NARROWEST: f32 = 50.0;
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

pub const PANELS: [Panel; 3] = [Panel::Slots, Panel::Wheel, Panel::Sliders];

impl Panel {
    fn min_size(self, axis: Axis) -> f32 {
        match (axis, self) {
            (Axis::Horizontal, Panel::Slots) => MIN_WIDTHS[0],
            (Axis::Horizontal, Panel::Wheel) => MIN_WIDTHS[1],
            (Axis::Horizontal, Panel::Sliders) => MIN_WIDTHS[2],
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

impl Axis {
    fn index(self) -> usize {
        match self {
            Axis::Horizontal => 0,
            Axis::Vertical => 1,
        }
    }

    fn other(self) -> Axis {
        match self {
            Axis::Horizontal => Axis::Vertical,
            Axis::Vertical => Axis::Horizontal,
        }
    }
}

/// Where something dropped on a stack of tabs goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Drop {
    /// Among its tabs, before the one at this index, or after them all if there is none.
    Tab(usize),
    /// Beside it, before it along the axis or after it.
    Side { axis: Axis, before: bool },
    /// Beside everything in the window: at the top level of its tree, and as a whole against
    /// what is there if that runs the other way.
    Around { axis: Axis, before: bool },
}

/// Where a hidden panel goes back to.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub struct Home {
    pub beside: Panel,
    pub drop: Drop,
    /// Its length, if the window gave that up when it was hidden and takes it back on return.
    pub size: Option<f32>,
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

    /// Takes a panel out and says how much less room this node needs now along each axis. All
    /// that is left keeps its length. The node must hold something else.
    pub fn lift(&mut self, panel: Panel) -> [f32; 2] {
        match self {
            Node::Tabs { panels, active } => {
                if let Some(index) = panels.iter().position(|&other| other == panel) {
                    panels.remove(index);
                    if *active > index || *active >= panels.len() {
                        *active = active.saturating_sub(1);
                    }
                }
                [0.0, 0.0]
            }
            Node::Split { axis, children } => {
                let axis = *axis;
                let Some(index) = children.iter().position(|child| child.node.panels().contains(&panel)) else {
                    return [0.0, 0.0];
                };
                let mut freed = [0.0, 0.0];
                if children[index].node.panels() == [panel] {
                    freed[axis.index()] = children.remove(index).size;
                } else {
                    freed = children[index].node.lift(panel);
                    children[index].size -= freed[axis.index()];
                    // What is left inside may now run the same way as this split.
                    if matches!(&children[index].node, Node::Split { axis: inner, .. } if *inner == axis) {
                        let child = children.remove(index);
                        children.splice(index..index, along_axis(child.node, axis, child.size));
                    }
                }
                if children.len() == 1 {
                    *self = children.remove(0).node;
                }
                freed
            }
        }
    }

    /// Takes a panel out, says where it goes back to and how much less room this node needs
    /// now, unless it is all the node holds.
    pub fn hide(&mut self, panel: Panel) -> Option<(Home, [f32; 2])> {
        let home = self.home(panel)?;
        let freed = self.lift(panel);
        Some((home, freed))
    }

    fn home(&self, panel: Panel) -> Option<Home> {
        match self {
            Node::Tabs { panels, .. } => {
                let index = panels.iter().position(|&other| other == panel)?;
                let beside = *panels.get(index.wrapping_sub(1)).or(panels.get(1))?;
                (panels.len() > 1).then_some(Home { beside, drop: Drop::Tab(index), size: None })
            }
            Node::Split { axis, children } => {
                let index = children.iter().position(|child| child.node.panels().contains(&panel))?;
                if children[index].node.panels().len() > 1 {
                    return children[index].node.home(panel);
                }
                let size = Some(children[index].size);
                // Beside all of a nested split, not one of the stacks in it.
                let home = |child: &Child, beside, before| {
                    let drop = match child.node {
                        Node::Tabs { .. } => Drop::Side { axis: *axis, before },
                        Node::Split { .. } => Drop::Around { axis: *axis, before },
                    };
                    Home { beside, drop, size }
                };
                let next = children.get(index + 1);
                let found = next.and_then(|child| Some(home(child, *child.node.panels().first()?, true)));
                found.or_else(|| {
                    let child = children.get(index.checked_sub(1)?)?;
                    Some(home(child, *child.node.panels().last()?, false))
                })
            }
        }
    }

    /// Puts a hidden panel back, and returns how much more room this node needs now. `room` is
    /// the area it fills at the moment.
    pub fn restore(&mut self, panel: Panel, home: Home, room: [f32; 2]) -> [f32; 2] {
        let new = Node::tabs(vec![panel]);
        match (home.drop, home.size) {
            (Drop::Side { axis, before }, Some(size)) => {
                let mut sizes = [0.0, 0.0];
                sizes[axis.index()] = size;
                self.insert(home.beside, axis, before, false, new, sizes, room)
            }
            (Drop::Around { axis, before }, Some(size)) => {
                let mut sizes = [0.0, 0.0];
                sizes[axis.index()] = size;
                self.insert(home.beside, axis, before, true, new, sizes, room)
            }
            _ => {
                self.dock(home.beside, home.drop, vec![panel]);
                [0.0, 0.0]
            }
        }
    }

    /// Puts `new` beside the stack that holds `target`, or if `around`, beside all there is,
    /// and returns how much more room this node needs now. Nothing has to give way: `new` keeps
    /// the width and height of `size`, as far as its minimums allow, and everything else keeps
    /// its own length. `room` is the area this node fills at the moment.
    #[allow(clippy::too_many_arguments)]
    pub fn insert(&mut self, target: Panel, axis: Axis, before: bool, around: bool, new: Node, size: [f32; 2], room: [f32; 2]) -> [f32; 2] {
        let across = axis.other();
        let length = size[axis.index()].max(new.min_size(axis));
        let mut more = [0.0, 0.0];
        more[axis.index()] = length;
        more[across.index()] = (size[across.index()].max(new.min_size(across)) - room[across.index()]).max(0.0);
        match self {
            Node::Split { axis: along, children } if *along == axis => {
                let Some(index) = children.iter().position(|child| child.node.panels().contains(&target)) else {
                    return [0.0, 0.0];
                };
                if around || matches!(children[index].node, Node::Tabs { .. }) {
                    let at = if before { index } else { index + 1 };
                    children.splice(at..at, along_axis(new, axis, length));
                    return more;
                }
                let child = &mut children[index];
                let mut inside = room;
                inside[axis.index()] = child.size;
                let more = child.node.insert(target, axis, before, around, new, size, inside);
                child.size += more[axis.index()];
                more
            }
            Node::Split { children, .. } if !around => {
                let Some(child) = children.iter_mut().find(|child| child.node.panels().contains(&target)) else {
                    return [0.0, 0.0];
                };
                let mut inside = room;
                inside[across.index()] = child.size;
                let more = child.node.insert(target, axis, before, around, new, size, inside);
                child.size += more[across.index()];
                more
            }
            _ => {
                let this = Child { node: std::mem::take(self), size: room[axis.index()] };
                let added = along_axis(new, axis, length);
                let children = if before {
                    added.into_iter().chain([this]).collect()
                } else {
                    std::iter::once(this).chain(added).collect()
                };
                *self = Node::Split { axis, children };
                more
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
            Drop::Side { axis, before } | Drop::Around { axis, before } => self.dock_beside(target, axis, before, panels),
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

    /// The minimum width and height of a window holding only this, as it is now. The colour
    /// slots can be low or narrow but not both, so each stops the window where the other has
    /// not made room yet. The window never changes size by itself on the way.
    pub fn min_at(&self, [width, height]: [f32; 2]) -> [f32; 2] {
        let [min_width, min_height] = [self.min_size(Axis::Horizontal), self.min_size(Axis::Vertical)];
        if !matches!(self, Node::Tabs { panels, .. } if panels[..] == [Panel::Slots]) {
            return [min_width, min_height];
        }
        [
            if height >= SLOTS_SQUARE_MIN_HEIGHT { SLOTS_NARROWEST } else { min_width },
            if width < min_width { SLOTS_SQUARE_MIN_HEIGHT } else { min_height },
        ]
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

/// A node as children of a split along `axis`: its own, if it is such a split itself.
fn along_axis(node: Node, axis: Axis, size: f32) -> Vec<Child> {
    match node {
        Node::Split { axis: inner, children } if inner == axis => children,
        node => vec![Child { node, size }],
    }
}

/// Whether every panel appears exactly once among these, which a damaged file can't be trusted
/// to do.
pub fn is_complete(nodes: &[&Node], hidden: &[Panel]) -> bool {
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
    let mut found = hidden.to_vec();
    nodes.iter().all(|node| collect(node, &mut found))
        && found.len() == PANELS.len()
        && PANELS.iter().all(|panel| found.contains(panel))
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
    fn lifting_a_panel_leaves_the_others_their_lengths_and_collapses_what_is_left() {
        let mut node = Node::stack([92.0, 360.0, 228.0]);
        assert_eq!(node.lift(Panel::Wheel), [0.0, 360.0]);
        assert_eq!(node.panels(), [Panel::Slots, Panel::Sliders]);
        assert_eq!(heights(&node), [92.0, 228.0]);
        assert_eq!(node.lift(Panel::Slots), [0.0, 92.0]);
        assert_eq!(node, Node::tabs(vec![Panel::Sliders]));
    }

    #[test]
    fn lifting_a_panel_out_of_a_nested_split_frees_room_along_that_split() {
        let mut node = nested();
        assert_eq!(node.lift(Panel::Slots), [0.0, 100.0]);
        assert_eq!(heights(&node), [300.0, 300.0]);
        assert_eq!(node.lift(Panel::Sliders), [300.0, 0.0]);
        assert_eq!(node, Node::tabs(vec![Panel::Wheel]));

        // A split left with nothing to divide hands its stacks to the one around it.
        let stack = |panel, size| Child { node: Node::tabs(vec![panel]), size };
        let row = Node::Split { axis: Axis::Horizontal, children: vec![stack(Panel::Slots, 300.0), stack(Panel::Wheel, 300.0)] };
        let mut node = Node::Split { axis: Axis::Vertical, children: vec![Child { node: row, size: 200.0 }, stack(Panel::Sliders, 100.0)] };
        assert_eq!(node.lift(Panel::Slots), [300.0, 0.0]);
        assert_eq!(heights(&node), [200.0, 100.0]);
        assert_eq!(node.panels(), [Panel::Wheel, Panel::Sliders]);
    }

    #[test]
    fn removing_a_tab_keeps_the_stack_and_a_valid_active_tab() {
        let mut node = Node::Tabs { panels: PANELS.to_vec(), active: 2 };
        node.lift(Panel::Sliders);
        assert_eq!(node, Node::Tabs { panels: vec![Panel::Slots, Panel::Wheel], active: 1 });
        node.lift(Panel::Slots);
        assert_eq!(node, Node::Tabs { panels: vec![Panel::Wheel], active: 0 });
    }

    #[test]
    fn a_panel_can_land_as_a_tab_or_on_either_side() {
        let mut node = Node::stack([92.0, 360.0, 228.0]);
        node.lift(Panel::Wheel);
        let mut tabbed = node.clone();
        tabbed.dock(Panel::Slots, Drop::Tab(usize::MAX), vec![Panel::Wheel]);
        assert_eq!(tabbed.panels(), [Panel::Slots, Panel::Wheel, Panel::Sliders]);
        assert!(is_complete(&[&tabbed], &[]));
        assert_eq!(heights(&tabbed), [92.0, 228.0]);

        let mut above = node.clone();
        let side = |before| Drop::Side { axis: Axis::Vertical, before };
        above.dock(Panel::Sliders, side(true), vec![Panel::Wheel]);
        assert_eq!(above.panels(), [Panel::Slots, Panel::Wheel, Panel::Sliders]);
        assert_eq!(heights(&above), [92.0, 114.0, 114.0]);

        let mut below = node;
        below.dock(Panel::Slots, side(false), vec![Panel::Wheel]);
        assert_eq!(below.panels(), [Panel::Slots, Panel::Wheel, Panel::Sliders]);
        assert_eq!(heights(&below), [46.0, 46.0, 228.0]);
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
        node.lift(Panel::Sliders);
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
    fn a_module_lands_with_its_own_length_and_the_room_it_takes_is_asked_for() {
        let mut node = Node::stack([92.0, 360.0, 228.0]);
        node.lift(Panel::Wheel);
        let wheel = Node::tabs(vec![Panel::Wheel]);
        let more = node.insert(Panel::Sliders, Axis::Vertical, false, false, wheel, [380.0, 300.0], [380.0, 320.0]);
        assert_eq!(more, [0.0, 300.0]);
        assert_eq!(heights(&node), [92.0, 228.0, 300.0]);
        assert_eq!(node.panels(), [Panel::Slots, Panel::Sliders, Panel::Wheel]);

        let mut lone = Node::tabs(vec![Panel::Wheel]);
        let slots = Node::tabs(vec![Panel::Slots]);
        let more = lone.insert(Panel::Wheel, Axis::Vertical, true, false, slots, [380.0, 92.0], [380.0, 360.0]);
        assert_eq!(more, [0.0, 92.0]);
        assert_eq!(heights(&lone), [92.0, 360.0]);
    }

    #[test]
    fn a_module_beside_one_of_a_stack_widens_the_window_and_raises_that_row_if_it_must() {
        let mut node = Node::stack([92.0, 360.0, 228.0]);
        node.lift(Panel::Wheel);
        let wheel = Node::tabs(vec![Panel::Wheel]);
        // Taller than the row it joins, which grows to its minimum height.
        let more = node.insert(Panel::Slots, Axis::Horizontal, false, false, wheel, [300.0, 200.0], [380.0, 320.0]);
        assert_eq!(more, [300.0, 128.0]);
        let Node::Split { axis: Axis::Vertical, children } = &node else { unreachable!() };
        assert_eq!(children[0].size, 220.0);

        // The only thing there: the wheel's own height decides.
        let mut node = Node::stack([92.0, 360.0, 228.0]);
        node.lift(Panel::Wheel);
        let wheel = Node::tabs(vec![Panel::Wheel]);
        node.lift(Panel::Sliders);
        let more = node.insert(Panel::Slots, Axis::Horizontal, true, false, wheel, [300.0, 360.0], [380.0, 92.0]);
        assert_eq!(more, [300.0, 268.0]);
        let Node::Split { axis: Axis::Horizontal, children } = &node else { unreachable!() };
        assert_eq!(children.iter().map(|child| child.size).collect::<Vec<_>>(), [300.0, 380.0]);
    }

    #[test]
    fn a_module_inside_a_nested_split_makes_the_stacks_around_it_longer_too() {
        let mut node = nested();
        let row = Node::tabs(vec![Panel::Slots]);
        node.lift(Panel::Slots);
        let more = node.insert(Panel::Sliders, Axis::Vertical, true, false, row, [300.0, 100.0], [600.0, 200.0]);
        assert_eq!(more, [0.0, 100.0]);
        assert_eq!(node, nested());
    }

    #[test]
    fn landing_around_everything_takes_the_whole_side_of_the_window() {
        let mut node = Node::stack([92.0, 360.0, 228.0]);
        node.lift(Panel::Wheel);
        let wheel = Node::tabs(vec![Panel::Wheel]);
        let more = node.insert(Panel::Slots, Axis::Horizontal, true, true, wheel, [300.0, 200.0], [380.0, 320.0]);
        assert_eq!(more, [300.0, 0.0]);
        let Node::Split { axis: Axis::Horizontal, children } = &node else { unreachable!() };
        assert_eq!(children[0].node, Node::tabs(vec![Panel::Wheel]));
        assert_eq!(children[1].size, 380.0);
        assert_eq!(children[1].node.panels(), [Panel::Slots, Panel::Sliders]);
    }

    #[test]
    fn a_split_landing_along_a_split_joins_it_as_equals() {
        let mut node = Node::tabs(vec![Panel::Wheel]);
        let stack = |panel, size| Child { node: Node::tabs(vec![panel]), size };
        let pair = Node::Split { axis: Axis::Vertical, children: vec![stack(Panel::Slots, 92.0), stack(Panel::Sliders, 200.0)] };
        node.insert(Panel::Wheel, Axis::Vertical, false, false, pair, [300.0, 292.0], [300.0, 360.0]);
        assert_eq!(heights(&node), [360.0, 92.0, 200.0]);
    }

    #[test]
    fn a_nested_split_fits_itself_and_counts_towards_its_parents_minimums() {
        let mut node = nested();
        assert_eq!(node.min_size(Axis::Horizontal), 410.0);
        // The taller of the wheel, and the slots and sliders together.
        assert_eq!(node.min_size(Axis::Vertical), 250.0);
        node.fit(800.0, 500.0);
        assert_eq!(heights(&node), [300.0, 500.0]);
        let Node::Split { children, .. } = &node else { unreachable!() };
        assert_eq!(heights(&children[1].node), [100.0, 400.0]);
    }

    #[test]
    fn the_slots_alone_go_narrow_only_while_tall_and_low_only_while_wide() {
        let slots = Node::tabs(vec![Panel::Slots]);
        assert_eq!(slots.min_at([300.0, 100.0]), [120.0, 80.0]);
        assert_eq!(slots.min_at([120.0, 100.0]), [120.0, 80.0]);
        assert_eq!(slots.min_at([300.0, 200.0]), [50.0, 80.0]);
        assert_eq!(slots.min_at([119.0, 300.0]), [50.0, 200.0]);
        let others = Node::tabs(vec![Panel::Slots, Panel::Wheel]);
        assert_eq!(others.min_at([300.0, 300.0]), [170.0, 220.0]);
    }

    #[test]
    fn only_a_split_along_the_axis_decides_its_total() {
        let node = nested();
        assert_eq!(node.total(Axis::Horizontal), Some(600.0));
        assert_eq!(node.total(Axis::Vertical), None);
    }

    #[test]
    fn every_panel_must_appear_exactly_once_among_the_windows() {
        assert!(is_complete(&[&Node::default()], &[]));
        assert!(is_complete(&[&nested()], &[]));
        let tabs = |panels: &[Panel]| Node::Tabs { panels: panels.to_vec(), active: 0 };
        assert!(is_complete(&[&tabs(&PANELS)], &[]));
        assert!(!is_complete(&[&tabs(&[Panel::Wheel, Panel::Wheel, Panel::Slots])], &[]));
        assert!(!is_complete(&[&tabs(&[Panel::Wheel])], &[]));
        assert!(!is_complete(&[&Node::Split { axis: Axis::Vertical, children: Vec::new() }], &[]));

        let (wheel, rest) = (tabs(&[Panel::Wheel]), tabs(&[Panel::Slots]));
        assert!(is_complete(&[&wheel, &rest], &[Panel::Sliders]));
        assert!(!is_complete(&[&wheel, &rest], &[Panel::Wheel]));
    }

    #[test]
    fn a_hidden_panel_comes_back_to_the_very_same_arrangement() {
        for panel in PANELS {
            let start = Node::stack([92.0, 360.0, 228.0]);
            let mut node = start.clone();
            let (home, _) = node.hide(panel).unwrap();
            assert!(!node.panels().contains(&panel));
            assert_eq!(home.size, Some(start_height(panel)));
            // The window gives up the length, so the others keep theirs.
            assert_eq!(node.total(Axis::Vertical), Some(680.0 - start_height(panel)));
            assert_eq!(node.restore(panel, home, [380.0, 680.0 - start_height(panel)]), [0.0, start_height(panel)]);
            assert_eq!(node, start);
        }
    }

    fn start_height(panel: Panel) -> f32 {
        match panel {
            Panel::Slots => 92.0,
            Panel::Wheel => 360.0,
            Panel::Sliders => 228.0,
        }
    }

    #[test]
    fn hiding_down_to_a_lone_panel_and_back_keeps_both_lengths() {
        let mut node = Node::stack([92.0, 360.0, 228.0]);
        let (sliders, _) = node.hide(Panel::Sliders).unwrap();
        let (slots, _) = node.hide(Panel::Slots).unwrap();
        assert_eq!(node, Node::tabs(vec![Panel::Wheel]));
        node.restore(Panel::Slots, slots, [380.0, 360.0]);
        assert_eq!(heights(&node), [92.0, 360.0]);
        node.restore(Panel::Sliders, sliders, [380.0, 452.0]);
        assert_eq!(node, Node::stack([92.0, 360.0, 228.0]));
    }

    #[test]
    fn a_hidden_panel_comes_back_to_a_nested_arrangement_too() {
        for panel in PANELS {
            let start = nested();
            let mut node = start.clone();
            let (home, freed) = node.hide(panel).unwrap();
            let room = [600.0 - freed[0], 300.0 - freed[1]];
            node.restore(panel, home, room);
            assert_eq!(node, start);
        }
    }

    #[test]
    fn a_hidden_tab_returns_to_its_place_and_the_last_panel_cannot_be_hidden() {
        let start = Node::Tabs { panels: PANELS.to_vec(), active: 1 };
        let mut node = start.clone();
        let (home, freed) = node.hide(Panel::Wheel).unwrap();
        assert_eq!((home.size, freed), (None, [0.0, 0.0]));
        node.restore(Panel::Wheel, home, [380.0, 360.0]);
        assert_eq!(node, start);

        let mut first = Node::tabs(PANELS.to_vec());
        let (home, _) = first.hide(Panel::Slots).unwrap();
        first.restore(Panel::Slots, home, [380.0, 360.0]);
        assert_eq!(first.panels(), PANELS);

        assert_eq!(Node::tabs(vec![Panel::Wheel]).hide(Panel::Wheel), None);
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
