//! Pure pane hierarchy and tab-bar geometry.
//!
//! IDs are unique within a tree. Tab groups are nonempty and their active index
//! is valid; the mutation methods preserve these invariants. A terminal has its
//! own empty tab bar, whose add button creates a nested group.

pub type Id = u64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Terminal(Id),
    Tabs {
        id: Id,
        children: Vec<Node>,
        active: usize,
    },
}

impl Node {
    pub fn id(&self) -> Id {
        match self {
            Self::Terminal(id) | Self::Tabs { id, .. } => *id,
        }
    }

    pub fn active_terminal(&self) -> Id {
        match self {
            Self::Terminal(id) => *id,
            Self::Tabs {
                children, active, ..
            } => children[*active].active_terminal(),
        }
    }

    fn find(&self, target: Id) -> Option<&Self> {
        if self.id() == target {
            return Some(self);
        }
        match self {
            Self::Terminal(_) => None,
            Self::Tabs { children, .. } => children.iter().find_map(|child| child.find(target)),
        }
    }

    fn find_mut(&mut self, target: Id) -> Option<&mut Self> {
        if self.id() == target {
            return Some(self);
        }
        match self {
            Self::Terminal(_) => None,
            Self::Tabs { children, .. } => {
                children.iter_mut().find_map(|child| child.find_mut(target))
            }
        }
    }

    fn terminals(&self, out: &mut Vec<Id>) {
        match self {
            Self::Terminal(id) => out.push(*id),
            Self::Tabs { children, .. } => {
                for child in children {
                    child.terminals(out);
                }
            }
        }
    }

    fn depths(&self, depth: usize, out: &mut Vec<(Id, usize)>) {
        match self {
            Self::Terminal(id) => out.push((*id, depth)),
            Self::Tabs { children, .. } => {
                for child in children {
                    child.depths(depth + 1, out);
                }
            }
        }
    }

    // Return the surviving subtree. Keeping groups with one child is deliberate:
    // their identity, tab bar, and terminal depths must not change on removal.
    fn remove(self, target: Id, removed: &mut Vec<Id>) -> Option<Self> {
        if self.id() == target {
            self.terminals(removed);
            return None;
        }
        match self {
            Self::Terminal(_) => Some(self),
            Self::Tabs {
                id,
                children,
                active,
            } => {
                let selected = children.get(active).map(Node::id);
                let children: Vec<_> = children
                    .into_iter()
                    .filter_map(|child| child.remove(target, removed))
                    .collect();
                if children.is_empty() {
                    return None;
                }
                // Retain the selected child's identity, not its old index. If
                // that child was closed, prefer the next sibling, then previous.
                let active = children
                    .iter()
                    .position(|child| Some(child.id()) == selected)
                    .unwrap_or_else(|| active.min(children.len() - 1));
                Some(Self::Tabs {
                    id,
                    children,
                    active,
                })
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    pub root: Option<Node>,
}

impl Tree {
    pub fn new(id: Id) -> Self {
        Self {
            root: Some(Node::Terminal(id)),
        }
    }

    pub fn active_terminal(&self) -> Option<Id> {
        self.root.as_ref().map(Node::active_terminal)
    }

    pub fn add(&mut self, target: Id, terminal: Id, group: Id) -> bool {
        let Some(node) = self.root.as_mut().and_then(|root| root.find_mut(target)) else {
            return false;
        };
        match node {
            Node::Terminal(id) => {
                *node = Node::Tabs {
                    id: group,
                    children: vec![Node::Terminal(*id), Node::Terminal(terminal)],
                    active: 1,
                };
            }
            Node::Tabs {
                children, active, ..
            } => {
                children.push(Node::Terminal(terminal));
                *active = children.len() - 1;
            }
        }
        true
    }

    pub fn select(&mut self, group: Id, child: Id) -> bool {
        let Some(Node::Tabs {
            children, active, ..
        }) = self.root.as_mut().and_then(|root| root.find_mut(group))
        else {
            return false;
        };
        let Some(index) = children.iter().position(|node| node.id() == child) else {
            return false;
        };
        *active = index;
        true
    }

    /// Resolve a zero-based sibling position without depending on clipped bars.
    pub fn tab_at(&self, group: Id, index: usize) -> Option<Id> {
        match self.root.as_ref()?.find(group)? {
            Node::Tabs { children, .. } => children.get(index).map(Node::id),
            Node::Terminal(_) => None,
        }
    }

    pub fn remove(&mut self, target: Id) -> Vec<Id> {
        let mut removed = Vec::new();
        self.root = self
            .root
            .take()
            .and_then(|root| root.remove(target, &mut removed));
        removed
    }

    pub fn terminals_in(&self, target: Id) -> Vec<Id> {
        let mut terminals = Vec::new();
        if let Some(node) = self.root.as_ref().and_then(|root| root.find(target)) {
            node.terminals(&mut terminals);
        }
        terminals
    }

    /// All leaves, including inactive leaves, with their full (unclipped) bar depth.
    pub fn terminal_depths(&self) -> Vec<(Id, usize)> {
        let mut depths = Vec::new();
        if let Some(root) = &self.root {
            root.depths(1, &mut depths);
        }
        depths
    }

    /// `top` and `rows` describe the remaining terminal viewport. At least one
    /// terminal row is reserved whenever the supplied height is nonzero.
    pub fn layout(&self, cols: u16, rows: u16) -> Layout {
        let mut bars = Vec::new();
        let mut node = self.root.as_ref();
        let bar_limit = usize::from(rows.saturating_sub(1));
        while let Some(current) = node {
            if bars.len() == bar_limit {
                break;
            }
            bars.push(Bar::new(current, bars.len() as u16, cols));
            node = match current {
                Node::Terminal(_) => None,
                Node::Tabs {
                    children, active, ..
                } => children.get(*active),
            };
        }
        let top = bars.len() as u16;
        Layout {
            bars,
            terminal: self.active_terminal(),
            top,
            rows: rows - top,
            cols,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub bars: Vec<Bar>,
    pub terminal: Option<Id>,
    pub top: u16,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bar {
    pub owner: Id,
    pub row: u16,
    pub tabs: Vec<Tab>,
    pub plus_x: u16,
    pub plus_width: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    pub id: Id,
    pub x: u16,
    pub width: u16,
    pub active: bool,
    pub label: String,
    /// First column of the three-column `[x]` control, when it fits.
    pub close_x: Option<u16>,
}

impl Bar {
    fn new(node: &Node, row: u16, cols: u16) -> Self {
        let plus_width = cols.min(3);
        let plus_x = cols - plus_width;
        let mut tabs = Vec::new();
        if let Node::Tabs {
            children, active, ..
        } = node
        {
            for (index, child) in children.iter().enumerate() {
                // Proportional boundaries distribute rounding remainders without
                // leaving gaps, including when some tabs have zero width.
                let x = (usize::from(plus_x) * index / children.len()) as u16;
                let end = (usize::from(plus_x) * (index + 1) / children.len()) as u16;
                let width = end - x;
                tabs.push(Tab {
                    id: child.id(),
                    x,
                    width,
                    active: index == *active,
                    // The tenth tab uses Option+0; show the actual shortcut.
                    label: if index == 9 { 0 } else { index + 1 }.to_string(),
                    close_x: if width >= 6 { Some(end - 3) } else { None },
                });
            }
        }
        Self {
            owner: node.id(),
            row,
            tabs,
            plus_x,
            plus_width,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Add(Id),
    Select { group: Id, child: Id },
    Close(Id),
}

impl Layout {
    /// Hit-test the same half-open column ranges exposed to the renderer.
    pub fn hit(&self, column: u16, row: u16) -> Option<Action> {
        if column >= self.cols {
            return None;
        }
        let bar = self.bars.iter().find(|bar| bar.row == row)?;
        if column >= bar.plus_x && column - bar.plus_x < bar.plus_width {
            return Some(Action::Add(bar.owner));
        }
        let tab = bar
            .tabs
            .iter()
            .find(|tab| column >= tab.x && column - tab.x < tab.width)?;
        if let Some(close_x) = tab.close_x
            && column >= close_x
            && column - close_x < 3
        {
            return Some(Action::Close(tab.id));
        }
        Some(Action::Select {
            group: bar.owner,
            child: tab.id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested() -> Tree {
        let mut tree = Tree::new(1);
        assert!(tree.add(1, 2, 10));
        assert!(tree.add(2, 3, 20));
        tree
    }

    #[test]
    fn nesting_preserves_terminal_ids_and_selection() {
        let mut tree = nested();
        assert_eq!(tree.root.as_ref().unwrap().id(), 10);
        assert_eq!(tree.terminals_in(10), vec![1, 2, 3]);
        assert_eq!(tree.terminals_in(20), vec![2, 3]);
        assert_eq!(tree.terminals_in(2), vec![2]);
        assert_eq!(tree.active_terminal(), Some(3));
        assert!(tree.select(10, 1));
        assert_eq!(tree.active_terminal(), Some(1));
        assert!(tree.select(20, 2)); // Hidden groups retain their own selection.
        assert_eq!(tree.active_terminal(), Some(1));
        assert!(tree.select(10, 20));
        assert_eq!(tree.active_terminal(), Some(2));
        assert!(tree.add(20, 4, 999)); // Existing groups do not use the new group ID.
        assert_eq!(tree.active_terminal(), Some(4));
        assert_eq!(tree.terminals_in(20), vec![2, 3, 4]);
        assert!(tree.terminals_in(999).is_empty());
    }

    #[test]
    fn missing_targets_and_nonchild_selections_are_noops() {
        let mut tree = nested();
        let before = tree.clone();
        assert!(!tree.add(999, 4, 30));
        assert!(!tree.select(999, 1));
        assert!(!tree.select(1, 1));
        assert!(!tree.select(10, 3));
        assert!(tree.remove(999).is_empty());
        assert!(tree.terminals_in(999).is_empty());
        assert_eq!(tree, before);
    }

    #[test]
    fn removal_preserves_selected_identity_and_single_child_groups() {
        let mut tree = nested();
        assert_eq!(tree.remove(1), vec![1]);
        assert_eq!(tree.active_terminal(), Some(3));
        assert_eq!(tree.root.as_ref().unwrap().id(), 10);
        assert_eq!(tree.terminal_depths(), vec![(2, 3), (3, 3)]);
        assert_eq!(tree.remove(2), vec![2]);
        assert_eq!(tree.active_terminal(), Some(3));
        assert_eq!(tree.terminal_depths(), vec![(3, 3)]);
        assert_eq!(tree.remove(3), vec![3]);
        assert_eq!(tree.root, None); // Empty groups recursively disappear.
        assert_eq!(tree.active_terminal(), None);
    }

    #[test]
    fn closing_selection_prefers_next_then_previous_sibling() {
        let mut tree = Tree::new(1);
        tree.add(1, 2, 10);
        tree.add(10, 3, 999);
        tree.select(10, 2);
        assert_eq!(tree.remove(2), vec![2]);
        assert_eq!(tree.active_terminal(), Some(3));
        assert_eq!(tree.remove(3), vec![3]);
        assert_eq!(tree.active_terminal(), Some(1));
        assert_eq!(tree.root.as_ref().unwrap().id(), 10);
    }

    #[test]
    fn subtree_and_root_removal_report_all_leaves_in_order() {
        let mut tree = nested();
        assert_eq!(tree.remove(20), vec![2, 3]);
        assert_eq!(tree.active_terminal(), Some(1));
        assert_eq!(tree.remove(10), vec![1]);
        assert_eq!(tree.root, None);
        assert_eq!(nested().remove(10), vec![1, 2, 3]);
        assert_eq!(Tree::new(42).remove(42), vec![42]);
    }

    #[test]
    fn active_bars_include_leaf_and_depths_include_hidden_branches() {
        let mut tree = nested();
        let layout = tree.layout(80, 24);
        assert_eq!(
            layout.bars.iter().map(|bar| bar.owner).collect::<Vec<_>>(),
            vec![10, 20, 3]
        );
        assert!(layout.bars[2].tabs.is_empty());
        assert_eq!(
            (layout.terminal, layout.top, layout.rows, layout.cols),
            (Some(3), 3, 21, 80)
        );
        assert_eq!(layout.bars[0].tabs[0].label, "1");
        assert_eq!(layout.bars[0].tabs[1].label, "2");
        assert_eq!(layout.bars[1].tabs[0].label, "1");
        assert_eq!(layout.bars[1].tabs[1].label, "2");
        assert!(!layout.bars[0].tabs[0].active);
        assert!(layout.bars[0].tabs[1].active);
        tree.select(10, 1);
        assert_eq!(
            tree.layout(80, 24)
                .bars
                .iter()
                .map(|bar| bar.owner)
                .collect::<Vec<_>>(),
            vec![10, 1]
        );
        assert_eq!(tree.terminal_depths(), vec![(1, 2), (2, 3), (3, 3)]);
        assert_eq!(Tree::new(7).terminal_depths(), vec![(7, 1)]);
    }

    #[test]
    fn small_heights_clip_bars_but_reserve_terminal_row_and_true_depth() {
        let tree = nested();
        for rows in 0..=5 {
            let layout = tree.layout(10, rows);
            let expected_top = rows.saturating_sub(1).min(3);
            assert_eq!(layout.top, expected_top);
            assert_eq!(layout.bars.len(), usize::from(expected_top));
            assert_eq!(layout.rows, rows - expected_top);
            assert_eq!(layout.terminal, Some(3));
            assert_eq!(tree.terminal_depths(), vec![(1, 2), (2, 3), (3, 3)]);
        }
    }

    #[test]
    fn proportional_widths_and_close_controls_have_exact_boundaries() {
        let mut tree = Tree::new(1);
        tree.add(1, 2, 10);
        tree.add(10, 3, 999);
        let layout = tree.layout(23, 5);
        let bar = &layout.bars[0];
        assert_eq!((bar.plus_x, bar.plus_width), (20, 3));
        assert_eq!(
            bar.tabs
                .iter()
                .map(|tab| (tab.x, tab.width, tab.close_x))
                .collect::<Vec<_>>(),
            vec![(0, 6, Some(3)), (6, 7, Some(10)), (13, 7, Some(17))]
        );
        assert_eq!(
            layout.hit(2, 0),
            Some(Action::Select {
                group: 10,
                child: 1
            })
        );
        assert_eq!(layout.hit(3, 0), Some(Action::Close(1)));
        assert_eq!(layout.hit(5, 0), Some(Action::Close(1)));
        assert_eq!(
            layout.hit(6, 0),
            Some(Action::Select {
                group: 10,
                child: 2
            })
        );
        assert_eq!(layout.hit(20, 0), Some(Action::Add(10)));
        assert_eq!(layout.hit(22, 1), Some(Action::Add(3)));
        assert_eq!(layout.hit(0, 1), None); // Empty leaf bar, not a selectable tab.
        assert_eq!(layout.hit(23, 0), None);
        assert_eq!(layout.hit(20, 2), None); // Terminal viewport.
    }

    #[test]
    fn hit_testing_matches_every_rendered_cell_even_at_narrow_dimensions() {
        let tree = nested();
        for cols in 0..=40 {
            for rows in 0..=5 {
                let layout = tree.layout(cols, rows);
                for bar in &layout.bars {
                    assert_eq!(bar.plus_x + bar.plus_width, cols);
                    let mut expected = vec![None; usize::from(cols)];
                    let mut next_x = 0;
                    for tab in &bar.tabs {
                        assert_eq!(tab.x, next_x);
                        next_x += tab.width;
                        assert_eq!(tab.close_x.is_some(), tab.width >= 6);
                        for x in tab.x..tab.x + tab.width {
                            expected[usize::from(x)] = Some(Action::Select {
                                group: bar.owner,
                                child: tab.id,
                            });
                        }
                        if let Some(x) = tab.close_x {
                            for x in x..x + 3 {
                                expected[usize::from(x)] = Some(Action::Close(tab.id));
                            }
                        }
                    }
                    if !bar.tabs.is_empty() {
                        assert_eq!(next_x, bar.plus_x);
                    }
                    for x in bar.plus_x..bar.plus_x + bar.plus_width {
                        expected[usize::from(x)] = Some(Action::Add(bar.owner));
                    }
                    for (x, action) in expected.into_iter().enumerate() {
                        assert_eq!(
                            layout.hit(x as u16, bar.row),
                            action,
                            "cols={cols}, row={}, x={x}",
                            bar.row
                        );
                    }
                    assert_eq!(layout.hit(cols, bar.row), None);
                }
                assert_eq!(layout.hit(0, layout.top), None);
            }
        }
    }

    #[test]
    fn empty_tree_and_maximum_dimensions_are_safe() {
        let mut tree = Tree::default();
        assert!(!tree.add(1, 2, 10));
        assert!(!tree.select(1, 2));
        assert!(tree.remove(1).is_empty());
        assert!(tree.terminals_in(1).is_empty());
        assert!(tree.terminal_depths().is_empty());
        let layout = tree.layout(80, 24);
        assert_eq!((layout.terminal, layout.top, layout.rows), (None, 0, 24));
        assert!(layout.bars.is_empty());
        assert_eq!(layout.hit(79, 0), None);
        let layout = nested().layout(u16::MAX, u16::MAX);
        assert_eq!(layout.rows, u16::MAX - 3);
        assert_eq!(layout.hit(u16::MAX - 1, 0), Some(Action::Add(10)));
    }
}
