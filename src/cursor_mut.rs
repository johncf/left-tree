use super::*;
use std::fmt;

// Note: The working of `CursorMut` is fundamentally different from `Cursor`. `CursorMut` can
//       become empty (iff `cur_node` is empty. `cur_node` empty implies `steps` is also empty).

/// A useful type alias for easy initialization of `CursorMut`.
pub type CursorMutT<L> = CursorMut<L, ()>;

/// A cursor object that can be used to modify internals of `Node` while maintaining balance.
///
/// `CursorMut` is heavier compared to `Cursor`. Even though `CursorMut` does not do any heap
/// allocations for its own operations, most operations tries to make the current node writable
/// using `Arc::make_mut`. This could result in a heap allocation if the number of references to
/// that node is more than one.
pub struct CursorMut<L: Leaf, P> {
    cur_node: Option<Node<L>>,
    steps: CVec<CursorMutStep<L, P>>,
}

struct CursorMutStep<L: Leaf, P> {
    nodes: RC<NVec<Node<L>>>,
    idx: usize,
    path_info: P,
}

impl<L, P> fmt::Debug for CursorMutStep<L, P> where L: Leaf, P: PathInfo<L::Info> + fmt::Debug {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "CursorMutStep {{ nodes.len: {}, idx: {}, path_info: {:?} }}",
                  self.nodes.len(), self.idx, self.path_info)
    }
}

impl<L, P> CursorMut<L, P> where L: Leaf, P: PathInfo<L::Info> {
    pub fn new() -> CursorMut<L, P> {
        CursorMut {
            cur_node: None,
            steps: CVec::new(),
        }
    }

    pub fn from_node(node: Node<L>) -> CursorMut<L, P> {
        CursorMut {
            cur_node: Some(node),
            steps: CVec::new(),
        }
    }

    pub fn into_root(mut self) -> Option<Node<L>> {
        self.reset();
        self.cur_node.take()
    }

    pub fn is_empty(&self) -> bool {
        self.cur_node.is_none()
    }

    pub fn current(&mut self) -> Option<&mut Node<L>> {
        self.cur_node.as_mut()
    }

    /// The cumulative info along the path from root to this node. Returns `P::identity()` if the
    /// current node is root.
    pub fn path_info(&self) -> P {
        match self.steps.last() {
            Some(cstep) => cstep.path_info,
            None => P::identity(),
        }
    }

    pub fn reset(&mut self) {
        while let Some(_) = self.ascend() {}
    }

    pub fn ascend(&mut self) -> Option<&mut Node<L>> {
        match self.cur_node.take() {
            Some(cur_node) => match self.steps.pop() {
                Some(CursorMutStep { mut nodes, idx, .. }) => {
                    RC::make_mut(&mut nodes).insert(idx, cur_node);
                    let parent = Node::from_children(nodes); // compute cumulative info
                    self.cur_node = Some(parent);
                    self.cur_node.as_mut()
                }
                None => { // cur_node is the root
                    self.cur_node = Some(cur_node);
                    None
                }
            },
            None => None, // cursor is empty
        }
    }

    pub fn descend_first(&mut self, idx: usize) -> Option<&mut Node<L>> {
        self.descend_extended(|_, _, i, _| i == idx, false)
    }

    pub fn descend_last(&mut self, idx: usize) -> Option<&mut Node<L>> {
        self.descend_extended(|_, _, _, i| i == idx, true)
    }

    /// Descend the tree once, on the child for which `f` returns `true`. It internally calls
    /// `descend_extended` for path info book-keeping.
    ///
    /// Returns `None` if `f` returned `false` on all children, or if it was a leaf node.
    ///
    /// The arguments to `f` are exactly the same as in [`Node::traverse`].
    ///
    /// [`Node::traverse`]: ../enum.Node.html#method.traverse
    pub fn descend<F>(&mut self, mut f: F, reversed: bool) -> Option<&mut Node<L>>
        where F: FnMut(L::Info, usize, usize) -> bool
    {
        self.descend_extended(|_, a, i, j| f(a, i, j), reversed)
    }

    /// Similar to descend, with the arguments to `f` treated exactly the same as in
    /// [`Node::path_traverse`].
    ///
    /// Panics if tree depth is greater than 8.
    pub fn descend_extended<F>(&mut self, f: F, reversed: bool) -> Option<&mut Node<L>>
        where F: FnMut(P, L::Info, usize, usize) -> bool
    {
        match self.cur_node.take() {
            Some(cur_node) => {
                let res = if reversed {
                    cur_node.path_traverse_rev(self.path_info(), f)
                } else {
                    cur_node.path_traverse(self.path_info(), f)
                }.map(|(index, path_info, _)| (index, path_info));

                match res {
                    Ok((index, path_info)) => {
                        self.descend_raw(cur_node.into_children_must(), index, path_info);
                        self.cur_node.as_mut()
                    }
                    Err(_) => {
                        self.cur_node = Some(cur_node);
                        None
                    },
                }
            }
            None => None, // empty cursor
        }
    }

    pub fn first_leaf_below(&mut self) -> Option<LeafMut<L>> {
        while let Some(_) = self.descend_first(0) {}
        self.current().map(|n| n.leaf_mut().unwrap())
    }

    // FIXME should the cursor should be guaranteed to be in the ancestor line after edit ops?

    /// Insert a leaf at the current position if currently focused on a leaf, or as the first leaf
    /// under the current node.
    ///
    /// It is unspecified where the cursor will be after this operation. The user should ensure
    /// that the cursor is at the correct location after this.
    ///
    /// _Side node:_ As per the current implementation, the cursor will either be pointing to the
    /// newly inserted node, or to an ancestor of the new node, or to one directly on the right of
    /// an ancestor node. But this behavior may change in future.
    pub fn insert(&mut self, leaf: L) {
        while let Some(_) = self.descend_first(0) {}
        self.insert_raw(Node::from_leaf(leaf), false);
    }

    /// Same as `insert` but insert after the current node (incl. all its leaf children).
    pub fn insert_after(&mut self, leaf: L) {
        while let Some(_) = self.descend_last(0) {}
        self.insert_raw(Node::from_leaf(leaf), true);
    }

    /// Remove the current node and return it. If the cursor is empty, return `None`.
    ///
    /// It is unspecified where the cursor will be after this operation. The user should ensure
    /// that the cursor is at the correct location after this.
    pub fn remove(&mut self) -> Option<Node<L>> {
        // the cursor should point to the same position if possible, or if there are no children on
        // the right to replace it, move left, or if it underflows, move up and merge with an
        // adjacent node.
        match self.cur_node.take() {
            Some(cur_node) => {
                self.fix_current();
                Some(cur_node)
            },
            None => None, // cursor is empty
        }
    }

    /// Split the tree into two, and return the right part of it. The current node, all leaves
    /// under it, as well as all leaves to the right of it will be included in the returned tree.
    pub fn split_off(&mut self) -> Node<L> {
        unimplemented!()
    }
}

impl<L, P> CursorMut<L, P> where L: Leaf, P: PathInfo<L::Info> {
    fn insert_raw(&mut self, newnode: Node<L>, after: bool) {
        match self.cur_node.take() {
            Some(cur_node) => {
                assert_eq!(cur_node.height(), newnode.height());
                match self.steps.pop() {
                    Some(mut cstep) => {
                        let _res = RC::make_mut(&mut cstep.nodes).insert(cstep.idx, cur_node);
                        debug_assert!(_res.is_none());
                        let newidx = if after { cstep.idx + 1 } else { cstep.idx };
                        let maybe_split = insert_maybe_split(RC::make_mut(&mut cstep.nodes), newidx, newnode);
                        if let Some(split_node) = maybe_split {
                            let parent = Node::from_children(cstep.nodes); // compute cumulative info
                            self.cur_node = Some(parent);
                            self.insert_raw(split_node, true);
                        } else {
                            let newnode = RC::make_mut(&mut cstep.nodes).remove(newidx);
                            debug_assert!(newnode.is_some());
                            self.cur_node = newnode;
                            cstep.idx = newidx;
                            self.steps.push(cstep);
                        }
                    }
                    None => { // cur_node is the root
                        self.cur_node = Some(if after {
                            Node::concat(cur_node, newnode)
                        } else {
                            Node::concat(newnode, cur_node)
                        });
                    }
                }
            }
            None => { // cursor was empty
                self.cur_node = Some(newnode);
            }
        }
    }

    // Find a replacement node for the current node. May ascend the tree multiple times.
    fn fix_current(&mut self) {
        if let Some(CursorMutStep { mut nodes, mut idx, mut path_info }) = self.steps.pop() {
            let nodes_len = nodes.len();
            let steps_len = self.steps.len();
            if nodes_len >= MIN_CHILDREN || (steps_len == 0 && nodes_len > 0) {
                if idx == nodes_len {
                    idx -= 1;
                }
                self.cur_node = RC::make_mut(&mut nodes).remove(idx);
                if nodes_len > 1 { // nodes is non-empty after remove
                    path_info = path_info.extend_inv(self.current().unwrap().info());
                    self.steps.push(CursorMutStep { nodes, idx, path_info });
                }
            } else if steps_len > 0 {
                debug_assert_eq!(nodes_len, MIN_CHILDREN - 1);
                self.cur_node = Some(Node::from_children(nodes));
                self.merge_adjacent();
            } else {
                unreachable!() // at no point should there be an empty nodes list in steps
            }
        } // else { the cursor became empty. nothing to do. }
    }

    // Merge the current node with an adjacent one to make it balanced.
    fn merge_adjacent(&mut self) {
        let CursorMutStep { mut nodes, mut idx, mut path_info } = self.steps.pop().unwrap();
        let mut cur_node = self.cur_node.take().unwrap();
        let merge_with_right = idx < nodes.len(); // merge with the right node by default
        debug_assert!(nodes.len() > 0);
        let merged;
        {
            let nodes = RC::make_mut(&mut nodes);
            merged = if merge_with_right {
                let right = nodes.get_mut(idx).unwrap();
                balance_maybe_merge(cur_node.children_mut_must(), right.children_mut_must())
            } else {
                let left = nodes.get_mut(idx - 1).unwrap();
                path_info = path_info.extend_inv(left.info());
                balance_maybe_merge(left.children_mut_must(), cur_node.children_mut_must())
            };
            if merged {
                if merge_with_right {
                    // swap in cur_node
                    std::mem::swap(&mut cur_node, nodes.get_mut(idx).unwrap());
                } else {
                    // cur_node is empty and idx == nodes.len()
                    idx -= 1;
                }
            } else {
                if !merge_with_right {
                    path_info = path_info.extend(nodes.get(idx - 1).unwrap().info());
                }
                self.cur_node = Some(cur_node);
            }
        };
        let _res = self.steps.push(CursorMutStep { nodes, idx, path_info });
        debug_assert!(_res.is_none());
        if merged { self.fix_current(); }
    }

    fn descend_raw(&mut self, mut nodes: RC<NVec<Node<L>>>, idx: usize, path_info: P) {
        debug_assert!(self.cur_node.is_none());
        let cur_node = RC::make_mut(&mut nodes).remove(idx).unwrap();
        self.cur_node = Some(cur_node);
        let _res = self.steps.push(CursorMutStep { nodes, idx, path_info });
        assert!(_res.is_none());
    }
}

impl<L, P> FromIterator<L> for CursorMut<L, P> where L: Leaf, P: PathInfo<L::Info> {
    fn from_iter<J: IntoIterator<Item=L>>(iter: J) -> Self {
        let mut curs = CursorMut::new();
        let mut iter = iter.into_iter().map(|e| Node::from_leaf(e));

        loop {
            loop {
                match curs.current().map(|node| node.height()) {
                    Some(h) if h > 1 => { curs.descend_last(0); }
                    _ => break,
                }
            }
            let nodes: NVec<_> = iter.by_ref().take(MAX_CHILDREN).collect();
            if nodes.len() > 0 {
                curs.insert_raw((Node::from_children(RC::new(nodes))), true);
            } else {
                break;
            }
        }
        curs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::tests::*;

    #[test]
    fn insert() {
        let mut cursor_mut = CursorMutT::new();
        for i in 0..128 {
            cursor_mut.insert_after(TestLeaf(i));
        }
        let root = cursor_mut.into_root().unwrap();
        let mut cursor = CursorT::new(&root);
        for i in 0..128 {
            assert_eq!(cursor.next_leaf(), Some(&TestLeaf(i)));
        }
        assert_eq!(cursor.next_leaf(), None);
    }

    #[test]
    fn delete() {
        let mut cursor_mut = CursorMutT::new();
        for i in 0..128 {
            cursor_mut.insert_after(TestLeaf(i));
        }
        for i in 0..128 {
            cursor_mut.reset();
            cursor_mut.first_leaf_below();
            println!("{}", i);
            assert_eq!(cursor_mut.remove().and_then(|n| n.into_leaf().ok()), Some(TestLeaf(i)));
        }
        assert_eq!(cursor_mut.is_empty(), true);
    }

    // FIXME need more tests
}
