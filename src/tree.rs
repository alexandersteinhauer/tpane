//! Pure session hierarchy, independent of terminal viewport geometry.
//!
//! IDs are unique; groups are nonempty and retain a valid active child.

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

    fn activate(&mut self, target: Id) -> bool {
        if self.id() == target {
            return true;
        }
        if let Self::Tabs {
            children, active, ..
        } = self
        {
            for (index, child) in children.iter_mut().enumerate() {
                if child.activate(target) {
                    *active = index;
                    return true;
                }
            }
        }
        false
    }

    fn parent(&self, target: Id) -> Option<Id> {
        if let Self::Tabs { id, children, .. } = self {
            if children.iter().any(|child| child.id() == target) {
                return Some(*id);
            }
            return children.iter().find_map(|child| child.parent(target));
        }
        None
    }

    // Keep single-child groups so their identity and navigation paths survive.
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
                // Preserve selected identity; on close prefer next, then previous.
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

    pub fn contains(&self, id: Id) -> bool {
        self.root.as_ref().and_then(|root| root.find(id)).is_some()
    }

    pub fn parent(&self, id: Id) -> Option<Id> {
        self.root.as_ref()?.parent(id)
    }

    /// Select the entire path to a node, retaining a group's remembered leaf.
    pub fn activate(&mut self, target: Id) -> bool {
        self.root.as_mut().is_some_and(|root| root.activate(target))
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Add(Id),
    Select { group: Id, child: Id },
    Close(Id),
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
        assert_eq!(tree.active_terminal(), Some(3));
        assert!(tree.select(10, 1));
        assert!(tree.select(20, 2)); // Hidden groups remember selection.
        assert_eq!(tree.active_terminal(), Some(1));
        assert!(tree.activate(20));
        assert_eq!(tree.active_terminal(), Some(2));
        assert!(tree.add(20, 4, 999));
        assert_eq!(tree.active_terminal(), Some(4));
        assert!(!tree.contains(999));
    }

    #[test]
    fn activate_selects_ancestors_and_parent_resolves_siblings() {
        let mut tree = nested();
        tree.activate(1);
        assert!(tree.activate(2));
        assert_eq!(tree.active_terminal(), Some(2));
        assert_eq!(tree.parent(2), Some(20));
        assert_eq!(tree.parent(20), Some(10));
        assert_eq!(tree.parent(10), None);
        assert_eq!(tree.parent(999), None);
        assert!(tree.contains(20));
    }

    #[test]
    fn missing_targets_and_nonchild_selections_are_noops() {
        let mut tree = nested();
        let before = tree.clone();
        assert!(!tree.add(999, 4, 30));
        assert!(!tree.select(999, 1));
        assert!(!tree.select(1, 1));
        assert!(!tree.select(10, 3));
        assert!(!tree.activate(999));
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
        assert_eq!(tree.remove(2), vec![2]);
        assert_eq!(tree.active_terminal(), Some(3));
        assert!(tree.contains(20));
        assert_eq!(tree.remove(3), vec![3]);
        assert_eq!(tree.root, None);
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
    fn empty_tree_is_safe() {
        let mut tree = Tree::default();
        assert!(!tree.add(1, 2, 10));
        assert!(!tree.select(1, 2));
        assert!(!tree.activate(1));
        assert!(tree.remove(1).is_empty());
        assert!(tree.terminals_in(1).is_empty());
        assert_eq!(tree.active_terminal(), None);
    }
}
