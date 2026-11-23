//! Position sets for TreeFeature's seven buckets. Java HashSet iteration is
//! observable here: a fresh iterator removes the first table/chain entry.
//! Tree bins preserve their linked iteration chain separately from the RB tree.

type Pos = (i32, i32, i32);
type Link = Option<usize>;

#[derive(Clone, Copy, Default)]
struct Bin {
    head: Link,
    tree: bool,
}
struct Node {
    pos: Pos,
    hash: u32,
    next: Link,
    prev: Link,
    parent: Link,
    left: Link,
    right: Link,
    red: bool,
}

pub(crate) struct LeafQueue {
    bins: Vec<Bin>,
    nodes: Vec<Node>,
    len: usize,
}
impl LeafQueue {
    pub(crate) fn new() -> Self {
        Self {
            bins: vec![Bin::default(); 16],
            nodes: Vec::new(),
            len: 0,
        }
    }
    fn hash(p: Pos) -> u32 {
        let h =
            p.1.wrapping_add(p.2.wrapping_mul(31))
                .wrapping_mul(31)
                .wrapping_add(p.0) as u32;
        h ^ (h >> 16)
    }
    fn red(&self, n: Link) -> bool {
        n.is_some_and(|n| self.nodes[n].red)
    }
    fn color(&mut self, n: Link, red: bool) {
        if let Some(n) = n {
            self.nodes[n].red = red;
        }
    }
    fn root(&self, mut n: usize) -> usize {
        while let Some(p) = self.nodes[n].parent {
            n = p;
        }
        n
    }
    fn rotate(&mut self, root: usize, p: usize, left: bool) -> usize {
        let r = if left {
            self.nodes[p].right
        } else {
            self.nodes[p].left
        }
        .unwrap();
        let inner = if left {
            self.nodes[r].left
        } else {
            self.nodes[r].right
        };
        if left {
            self.nodes[p].right = inner;
        } else {
            self.nodes[p].left = inner;
        }
        if let Some(i) = inner {
            self.nodes[i].parent = Some(p);
        }
        let parent = self.nodes[p].parent;
        self.nodes[r].parent = parent;
        let root = if let Some(parent) = parent {
            if self.nodes[parent].left == Some(p) {
                self.nodes[parent].left = Some(r);
            } else {
                self.nodes[parent].right = Some(r);
            }
            root
        } else {
            self.nodes[r].red = false;
            r
        };
        if left {
            self.nodes[r].left = Some(p);
        } else {
            self.nodes[r].right = Some(p);
        }
        self.nodes[p].parent = Some(r);
        root
    }
    fn balance_insert(&mut self, mut root: usize, mut x: usize) -> usize {
        self.nodes[x].red = true;
        loop {
            let Some(p) = self.nodes[x].parent else {
                self.nodes[x].red = false;
                return x;
            };
            let Some(g) = self.nodes[p].parent else {
                return root;
            };
            if !self.nodes[p].red {
                return root;
            }
            let left = self.nodes[g].left == Some(p);
            let uncle = if left {
                self.nodes[g].right
            } else {
                self.nodes[g].left
            };
            if self.red(uncle) {
                self.color(uncle, false);
                self.nodes[p].red = false;
                self.nodes[g].red = true;
                x = g;
            } else {
                let mut parent = p;
                if (if left {
                    self.nodes[p].right
                } else {
                    self.nodes[p].left
                }) == Some(x)
                {
                    x = p;
                    root = self.rotate(root, x, left);
                    parent = self.nodes[x].parent.unwrap();
                }
                let grand = self.nodes[parent].parent.unwrap();
                self.nodes[parent].red = false;
                self.nodes[grand].red = true;
                return self.rotate(root, grand, !left);
            }
        }
    }
    fn balance_delete(&mut self, mut root: usize, mut x: usize) -> usize {
        loop {
            if x == root {
                return root;
            }
            let Some(mut p) = self.nodes[x].parent else {
                self.nodes[x].red = false;
                return x;
            };
            if self.nodes[x].red {
                self.nodes[x].red = false;
                return root;
            }
            let left = self.nodes[p].left == Some(x);
            let mut sibling = if left {
                self.nodes[p].right
            } else {
                self.nodes[p].left
            };
            if self.red(sibling) {
                self.color(sibling, false);
                self.nodes[p].red = true;
                root = self.rotate(root, p, left);
                p = self.nodes[x].parent.unwrap();
                sibling = if left {
                    self.nodes[p].right
                } else {
                    self.nodes[p].left
                };
            }
            let Some(s) = sibling else {
                x = p;
                continue;
            };
            let near = if left {
                self.nodes[s].left
            } else {
                self.nodes[s].right
            };
            let far = if left {
                self.nodes[s].right
            } else {
                self.nodes[s].left
            };
            if !self.red(near) && !self.red(far) {
                self.nodes[s].red = true;
                x = p;
            } else {
                if !self.red(far) {
                    self.color(near, false);
                    self.nodes[s].red = true;
                    root = self.rotate(root, s, !left);
                    sibling = if left {
                        self.nodes[p].right
                    } else {
                        self.nodes[p].left
                    };
                }
                if let Some(s) = sibling {
                    self.nodes[s].red = self.nodes[p].red;
                    let far = if left {
                        self.nodes[s].right
                    } else {
                        self.nodes[s].left
                    };
                    self.color(far, false);
                }
                self.nodes[p].red = false;
                root = self.rotate(root, p, left);
                x = root;
            }
        }
    }
    fn move_root(&mut self, bin: usize, root: usize) {
        let first = self.bins[bin].head;
        if first == Some(root) {
            return;
        }
        let prev = self.nodes[root].prev;
        let next = self.nodes[root].next;
        if let Some(prev) = prev {
            self.nodes[prev].next = next;
        }
        if let Some(next) = next {
            self.nodes[next].prev = prev;
        }
        self.nodes[root].prev = None;
        self.nodes[root].next = first;
        if let Some(first) = first {
            self.nodes[first].prev = Some(root);
        }
        self.bins[bin].head = Some(root);
    }
    fn compare_hash(&self, a: usize, b: usize) -> std::cmp::Ordering {
        let order = (self.nodes[a].hash as i32).cmp(&(self.nodes[b].hash as i32));
        assert!(!order.is_eq(), "native leaf tree-bin identity-hash tie is not a deterministic coordinate order: {:?} / {:?}", self.nodes[a].pos, self.nodes[b].pos);
        order
    }
    fn treeify(&mut self, bin: usize) {
        let mut root = None;
        let mut next = self.bins[bin].head;
        while let Some(n) = next {
            next = self.nodes[n].next;
            self.nodes[n].parent = None;
            self.nodes[n].left = None;
            self.nodes[n].right = None;
            self.nodes[n].red = false;
            if let Some(mut p) = root {
                loop {
                    let left = self.compare_hash(n, p).is_lt();
                    let child = if left {
                        self.nodes[p].left
                    } else {
                        self.nodes[p].right
                    };
                    if let Some(child) = child {
                        p = child;
                        continue;
                    }
                    self.nodes[n].parent = Some(p);
                    if left {
                        self.nodes[p].left = Some(n);
                    } else {
                        self.nodes[p].right = Some(n);
                    }
                    root = Some(self.balance_insert(root.unwrap(), n));
                    break;
                }
            } else {
                root = Some(n);
            }
        }
        self.bins[bin].tree = true;
        if let Some(root) = root {
            self.move_root(bin, root);
        }
    }
    fn grow(&mut self) {
        let old_len = self.bins.len();
        let old = std::mem::replace(&mut self.bins, vec![Bin::default(); old_len * 2]);
        for (i, bin) in old.into_iter().enumerate() {
            let mut groups = [Vec::new(), Vec::new()];
            let mut n = bin.head;
            while let Some(j) = n {
                n = self.nodes[j].next;
                groups[usize::from(self.nodes[j].hash as usize & old_len != 0)].push(j);
            }
            for side in 0..2 {
                let ids = &groups[side];
                let index = i + side * old_len;
                self.bins[index].head = ids.first().copied();
                self.bins[index].tree = bin.tree && ids.len() > 6;
                for (k, &n) in ids.iter().enumerate() {
                    self.nodes[n].prev = k.checked_sub(1).map(|k| ids[k]);
                    self.nodes[n].next = ids.get(k + 1).copied();
                    if !self.bins[index].tree {
                        self.nodes[n].parent = None;
                        self.nodes[n].left = None;
                        self.nodes[n].right = None;
                    }
                }
                if self.bins[index].tree && !groups[1 - side].is_empty() {
                    self.treeify(index);
                }
            }
        }
    }
    pub(crate) fn insert(&mut self, pos: Pos) {
        let hash = Self::hash(pos);
        let bin = hash as usize & (self.bins.len() - 1);
        let mut last = None;
        let mut count = 0;
        let mut cursor = self.bins[bin].head;
        let mut tree_parent = None;
        if self.bins[bin].tree {
            cursor = cursor.map(|n| self.root(n));
            while let Some(n) = cursor {
                if self.nodes[n].pos == pos {
                    return;
                }
                let order = (hash as i32).cmp(&(self.nodes[n].hash as i32));
                assert!(
                    !order.is_eq(),
                    "native leaf tree-bin identity-hash tie: {:?} / {:?}",
                    pos,
                    self.nodes[n].pos
                );
                tree_parent = Some((n, order.is_lt()));
                cursor = if order.is_lt() {
                    self.nodes[n].left
                } else {
                    self.nodes[n].right
                };
            }
        } else {
            while let Some(n) = cursor {
                if self.nodes[n].pos == pos {
                    return;
                }
                last = Some(n);
                count += 1;
                cursor = self.nodes[n].next;
            }
        }
        let n = self.nodes.len();
        self.nodes.push(Node {
            pos,
            hash,
            next: None,
            prev: None,
            parent: None,
            left: None,
            right: None,
            red: false,
        });
        if let Some((p, left)) = tree_parent {
            let root = self.root(p);
            let next = self.nodes[p].next;
            self.nodes[n].parent = Some(p);
            self.nodes[n].prev = Some(p);
            self.nodes[n].next = next;
            self.nodes[p].next = Some(n);
            if let Some(next) = next {
                self.nodes[next].prev = Some(n);
            }
            if left {
                self.nodes[p].left = Some(n);
            } else {
                self.nodes[p].right = Some(n);
            }
            let root = self.balance_insert(root, n);
            self.move_root(bin, root);
        } else {
            if let Some(last) = last {
                self.nodes[last].next = Some(n);
                self.nodes[n].prev = Some(last);
            } else {
                self.bins[bin].head = Some(n);
            }
            if count >= 8 {
                if self.bins.len() < 64 {
                    self.grow();
                } else {
                    self.treeify(bin);
                }
            }
        }
        self.len += 1;
        if self.len > self.bins.len() * 3 / 4 {
            self.grow();
        }
    }
    fn remove_tree(&mut self, p: usize, mut root: usize) {
        let left = self.nodes[p].left;
        let right = self.nodes[p].right;
        let replacement;
        if let (Some(left), Some(right)) = (left, right) {
            let mut s = right;
            while let Some(left) = self.nodes[s].left {
                s = left;
            }
            let red = self.nodes[s].red;
            self.nodes[s].red = self.nodes[p].red;
            self.nodes[p].red = red;
            let sr = self.nodes[s].right;
            let pp = self.nodes[p].parent;
            if s == right {
                self.nodes[p].parent = Some(s);
                self.nodes[s].right = Some(p);
            } else {
                let sp = self.nodes[s].parent;
                self.nodes[p].parent = sp;
                if let Some(sp) = sp {
                    if self.nodes[sp].left == Some(s) {
                        self.nodes[sp].left = Some(p);
                    } else {
                        self.nodes[sp].right = Some(p);
                    }
                }
                self.nodes[s].right = Some(right);
                self.nodes[right].parent = Some(s);
            }
            self.nodes[p].left = None;
            self.nodes[p].right = sr;
            if let Some(sr) = sr {
                self.nodes[sr].parent = Some(p);
            }
            self.nodes[s].left = Some(left);
            self.nodes[left].parent = Some(s);
            self.nodes[s].parent = pp;
            if let Some(pp) = pp {
                if self.nodes[pp].left == Some(p) {
                    self.nodes[pp].left = Some(s);
                } else {
                    self.nodes[pp].right = Some(s);
                }
            } else {
                root = s;
            }
            replacement = sr.unwrap_or(p);
        } else {
            replacement = left.or(right).unwrap_or(p);
        }
        if replacement != p {
            let pp = self.nodes[p].parent;
            self.nodes[replacement].parent = pp;
            if let Some(pp) = pp {
                if self.nodes[pp].left == Some(p) {
                    self.nodes[pp].left = Some(replacement);
                } else {
                    self.nodes[pp].right = Some(replacement);
                }
            } else {
                root = replacement;
            }
            self.nodes[p].left = None;
            self.nodes[p].right = None;
            self.nodes[p].parent = None;
        }
        if !self.nodes[p].red {
            self.balance_delete(root, replacement);
        }
        if replacement == p {
            if let Some(pp) = self.nodes[p].parent {
                if self.nodes[pp].left == Some(p) {
                    self.nodes[pp].left = None;
                } else if self.nodes[pp].right == Some(p) {
                    self.nodes[pp].right = None;
                }
            }
            self.nodes[p].parent = None;
        }
    }
    pub(crate) fn pop(&mut self) -> Option<Pos> {
        let bin = self.bins.iter().position(|b| b.head.is_some())?;
        let n = self.bins[bin].head.unwrap();
        let root = if self.bins[bin].tree {
            Some(self.root(n))
        } else {
            None
        };
        self.bins[bin].head = self.nodes[n].next;
        if let Some(next) = self.nodes[n].next {
            self.nodes[next].prev = None;
        }
        self.len -= 1;
        if self.bins[bin].head.is_none() {
            self.bins[bin].tree = false;
        } else if let Some(root) = root {
            self.remove_tree(n, root);
        }
        // Iterator.remove passes movable=false: it must not move the new root
        // to the front of the iteration chain or untreeify a small live tree.
        Some(self.nodes[n].pos)
    }
}

#[cfg(all(test, megu_vanilla_data))]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn point(s: &str) -> Pos {
        let a: Vec<i32> = s.split(',').map(|v| v.parse().unwrap()).collect();
        assert_eq!(a.len(), 3);
        (a[0], a[1], a[2])
    }
    fn check(q: &LeafQueue) {
        fn visit(q: &LeafQueue, n: Link, parent: Link, seen: &mut HashSet<usize>) {
            let Some(n) = n else { return };
            assert!(seen.insert(n), "tree cycle or duplicate");
            let node = &q.nodes[n];
            assert_eq!(node.parent, parent);
            if let Some(left) = node.left {
                assert!((q.nodes[left].hash as i32) < node.hash as i32);
            }
            if let Some(right) = node.right {
                assert!((q.nodes[right].hash as i32) > node.hash as i32);
            }
            visit(q, node.left, Some(n), seen);
            visit(q, node.right, Some(n), seen);
        }
        let mut total = 0;
        for (index, bin) in q.bins.iter().enumerate() {
            let mut chain = HashSet::new();
            let mut previous = None;
            let mut n = bin.head;
            while let Some(i) = n {
                assert!(chain.insert(i), "iteration chain cycle");
                assert_eq!(q.nodes[i].prev, previous);
                assert_eq!(q.nodes[i].hash as usize & (q.bins.len() - 1), index);
                previous = n;
                n = q.nodes[i].next;
            }
            total += chain.len();
            if bin.tree {
                let mut root = bin.head.unwrap();
                let mut steps = 0;
                while let Some(p) = q.nodes[root].parent {
                    root = p;
                    steps += 1;
                    assert!(steps <= chain.len());
                }
                // Native iterator removal retains tiny tree bins and can
                // return a red root. Check structure here; actual JDK pop
                // sequences, not stricter textbook colors, are the oracle.
                let mut tree = HashSet::new();
                visit(q, Some(root), None, &mut tree);
                assert_eq!(
                    tree, chain,
                    "tree and iteration chain contain the same nodes"
                );
            } else {
                for n in chain {
                    assert_eq!(q.nodes[n].parent, None);
                }
            }
        }
        assert_eq!(q.len, total);
    }
    #[test]
    #[should_panic(expected = "identity-hash tie")]
    fn an_unreplayable_java_identity_tie_is_not_coordinate_sorted() {
        let mut q = LeafQueue::new();
        for x in 0..12 {
            q.insert((64 * x, 0, 0));
        }
        // Distinct BlockPos values with identical full hash. Java tree bins
        // then consult JVM object identity, which is not a coordinate input.
        q.insert((31, -1, 0));
    }

    #[test]
    fn iteration_and_iterator_remove_match_native_hashset() {
        let mut seen = HashSet::new();
        let mut tree_steps = 0;
        let mut tree_splits = 0;
        for row in include_str!("../tests/data/leaf_queue_1211.tsv").lines() {
            let f: Vec<_> = row.split('\t').collect();
            assert_eq!(f.len(), 4);
            assert!(seen.insert((f[0], f[1])));
            let origin = point(f[1]);
            let mut q = LeafQueue::new();
            let mut expected = f[3].split(';');
            for op in f[2].split(';') {
                let old_cap = q.bins.len();
                let was_tree = q.bins.iter().any(|b| b.tree);
                if op == "p" {
                    let result = q
                        .pop()
                        .map(|p| (p.0 - origin.0, p.1 - origin.1, p.2 - origin.2));
                    let want = expected.next().unwrap();
                    assert_eq!(
                        result,
                        if want == "empty" {
                            None
                        } else {
                            Some(point(want))
                        },
                        "native queue {} {}",
                        f[0],
                        f[1]
                    );
                } else {
                    let p = point(op.strip_prefix('+').unwrap());
                    q.insert((p.0 + origin.0, p.1 + origin.1, p.2 + origin.2));
                }
                tree_steps += usize::from(q.bins.iter().any(|b| b.tree));
                tree_splits += usize::from(was_tree && q.bins.len() > old_cap);
                check(&q);
            }
            assert!(expected.next().is_none());
            assert_eq!(q.len, 0);
        }
        assert_eq!(seen.len(), 15);
        assert!(
            tree_steps > 0 && tree_splits > 0,
            "native tree-bin and resize stimuli must execute"
        );
    }
}
