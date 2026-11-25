//! Which title buffer holds a range of addresses, for [`crate::shadow`].
//!
//! Buffers nest and overlap (a title places small buffers inside a big
//! heap's range), and a lookup wants the one that starts last at or below
//! an address and still reaches the range's end. Walking the buffers down
//! from the address finds it, but has no bound a title cannot pass: every
//! buffer below the address that ends too early is looked at, and a range
//! in no buffer looks at all of them, on every draw.
//!
//! [`Holders`] is a search tree over (start, resource) in which every node
//! also knows the furthest end in its subtree, so a subtree that cannot
//! hold the range is skipped whole. A lookup then looks at a number of
//! nodes in proportion to the tree's depth, and says how many it looked at
//! ([`Found::looked_at`]).
//!
//! The tree is a treap: a node's priority is a fixed mix of its key, so
//! the shape depends only on the set of keys, and for keys that are not
//! chosen against the mix the depth stays within a small multiple of the
//! logarithm of their number. No rebalancing is done beyond that.

#![cfg_attr(not(windows), allow(dead_code))]

/// A buffer's key: where it starts and the resource it is.
pub(crate) type Key = (u64, usize);

struct Node {
    key: Key,
    end: u64,
    priority: u64,
    /// The furthest `end` of this node and all below it.
    furthest: u64,
    left: Tree,
    right: Tree,
}

type Tree = Option<Box<Node>>;

/// What a lookup found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Found {
    /// The buffer, if one holds the range.
    pub(crate) key: Option<Key>,
    /// How many buffers were looked at to tell.
    pub(crate) looked_at: usize,
}

/// Title buffers by key, each with its end.
#[derive(Default)]
pub(crate) struct Holders {
    root: Tree,
    len: usize,
}

/// A node's priority: splitmix64's finishing steps over both halves of the
/// key. The same key always gets the same one.
fn priority(key: Key) -> u64 {
    let mut x = key
        .0
        .wrapping_add((key.1 as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

fn furthest(tree: &Tree) -> Option<u64> {
    tree.as_ref().map(|n| n.furthest)
}

fn settle(mut node: Box<Node>) -> Box<Node> {
    node.furthest = node
        .end
        .max(furthest(&node.left).unwrap_or(0))
        .max(furthest(&node.right).unwrap_or(0));
    node
}

/// `tree` as the keys below `key` and the keys from `key` on.
fn split(tree: Tree, key: Key) -> (Tree, Tree) {
    let Some(mut node) = tree else {
        return (None, None);
    };
    if node.key < key {
        let (low, high) = split(node.right.take(), key);
        node.right = low;
        (Some(settle(node)), high)
    } else {
        let (low, high) = split(node.left.take(), key);
        node.left = high;
        (low, Some(settle(node)))
    }
}

/// One tree of two, every key of `low` below every key of `high`.
fn merge(low: Tree, high: Tree) -> Tree {
    match (low, high) {
        (None, tree) | (tree, None) => tree,
        (Some(mut a), Some(mut b)) => {
            if a.priority >= b.priority {
                a.right = merge(a.right.take(), Some(b));
                Some(settle(a))
            } else {
                b.left = merge(Some(a), b.left.take());
                Some(settle(b))
            }
        }
    }
}

/// `tree` without `key`, and whether it was there.
fn without(tree: Tree, key: Key) -> (Tree, bool) {
    let Some(mut node) = tree else {
        return (None, false);
    };
    match key.cmp(&node.key) {
        std::cmp::Ordering::Less => {
            let (left, was) = without(node.left.take(), key);
            node.left = left;
            (Some(settle(node)), was)
        }
        std::cmp::Ordering::Greater => {
            let (right, was) = without(node.right.take(), key);
            node.right = right;
            (Some(settle(node)), was)
        }
        std::cmp::Ordering::Equal => (merge(node.left.take(), node.right.take()), true),
    }
}

/// The last key at or below `limit` whose buffer ends at or past `end`.
fn last_reaching(tree: &Tree, limit: Key, end: u64, looked_at: &mut usize) -> Option<Key> {
    let node = tree.as_ref()?;
    *looked_at += 1;
    if node.furthest < end {
        return None;
    }
    if node.key > limit {
        return last_reaching(&node.left, limit, end, looked_at);
    }
    // Later keys first. Only along the path to `limit` can a subtree that
    // reaches `end` still have nothing: all of any other is below `limit`.
    if let Some(key) = last_reaching(&node.right, limit, end, looked_at) {
        return Some(key);
    }
    if node.end >= end {
        return Some(node.key);
    }
    last_reaching(&node.left, limit, end, looked_at)
}

impl Holders {
    pub(crate) const fn new() -> Holders {
        Holders { root: None, len: 0 }
    }

    /// How many buffers there are.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// A buffer `key` that ends at `end` (its end from now on, if it was
    /// there already).
    pub(crate) fn insert(&mut self, key: Key, end: u64) {
        self.remove(key);
        let node = Box::new(Node {
            key,
            end,
            priority: priority(key),
            furthest: end,
            left: None,
            right: None,
        });
        let (low, high) = split(self.root.take(), key);
        self.root = merge(merge(low, Some(node)), high);
        self.len += 1;
    }

    pub(crate) fn remove(&mut self, key: Key) {
        let (root, was) = without(self.root.take(), key);
        self.root = root;
        self.len -= usize::from(was);
    }

    /// The buffer that starts last at or below `address` (of those that
    /// start there, the one with the highest resource) and ends at or past
    /// `end`: the innermost one holding `[address, end)`.
    pub(crate) fn holder(&self, address: u64, end: u64) -> Found {
        let mut looked_at = 0;
        let key = last_reaching(&self.root, (address, usize::MAX), end, &mut looked_at);
        Found { key, looked_at }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// The walk [`Holders`] replaces: down from the address, every buffer
    /// in turn. The answer lookups must give, and what it costs.
    fn walked(all: &BTreeMap<Key, u64>, address: u64, end: u64) -> Found {
        let mut looked_at = 0;
        let key = all
            .range(..=(address, usize::MAX))
            .rev()
            .inspect(|_| looked_at += 1)
            .find(|(_, e)| **e >= end)
            .map(|(k, _)| *k);
        Found { key, looked_at }
    }

    /// Numbers that differ from run to run of the loop, the same in every
    /// run of the test.
    struct Seeded(u64);

    impl Seeded {
        fn next(&mut self, below: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % below
        }
    }

    fn depth(tree: &Tree) -> usize {
        tree.as_ref()
            .map_or(0, |n| 1 + depth(&n.left).max(depth(&n.right)))
    }

    /// What a lookup may look at among `n` buffers: a small multiple of
    /// log2(n), far below `n` for the numbers the tests use.
    fn allowed(n: usize) -> usize {
        8 * (usize::BITS - n.leading_zeros()) as usize + 8
    }

    #[test]
    fn the_innermost_holder_is_found() {
        let mut holders = Holders::new();
        holders.insert((0x1000, 1), 0x3000);
        holders.insert((0x1400, 2), 0x1600);
        let key = |address, end| holders.holder(address, end).key;
        assert_eq!(key(0x1410, 0x1430), Some((0x1400, 2)));
        assert_eq!(key(0x1400, 0x1600), Some((0x1400, 2)));
        assert_eq!(key(0x1400, 0x1601), Some((0x1000, 1)));
        assert_eq!(key(0x13ff, 0x1401), Some((0x1000, 1)));
        assert_eq!(key(0x2000, 0x3000), Some((0x1000, 1)));
        assert_eq!(key(0x2000, 0x3001), None);
        assert_eq!(key(0xfff, 0x1001), None);
        assert_eq!(key(u64::MAX, u64::MAX), None);
        assert_eq!(holders.len(), 2);
        // Two resources at one address: the higher one, as the walk gave.
        holders.insert((0x1400, 7), 0x1600);
        assert_eq!(key_of(&holders, 0x1410, 0x1430), Some((0x1400, 7)));
        // A buffer registered again has its new end only.
        holders.insert((0x1400, 7), 0x1500);
        assert_eq!(holders.len(), 3);
        assert_eq!(key_of(&holders, 0x1410, 0x1530), Some((0x1400, 2)));
        holders.remove((0x1400, 2));
        holders.remove((0x1400, 2));
        assert_eq!(holders.len(), 2);
        assert_eq!(key_of(&holders, 0x1410, 0x1530), Some((0x1000, 1)));
        holders.remove((0x1000, 1));
        assert_eq!(key_of(&holders, 0x1410, 0x1530), None);
        assert_eq!(key_of(&holders, 0x1410, 0x1500), Some((0x1400, 7)));
    }

    fn key_of(holders: &Holders, address: u64, end: u64) -> Option<Key> {
        holders.holder(address, end).key
    }

    /// The case the 64-buffer limit was lifted for, at a size where the
    /// walk that replaced it is the problem: one big buffer with 20 000
    /// small ones inside it. A range only the big one holds, and a range
    /// nothing holds, used to look at every small buffer below them.
    #[test]
    fn a_lookup_past_many_buffers_looks_at_few() {
        const BASE: u64 = 0x7a00_0000_0000;
        const SMALL: u64 = 20_000;
        let mut holders = Holders::new();
        let mut all = BTreeMap::new();
        let mut add = |key: Key, end: u64| {
            holders.insert(key, end);
            all.insert(key, end);
        };
        add((BASE, 1), BASE + (64 << 20));
        for i in 0..SMALL {
            add((BASE + 16 * i, 2 + i as usize), BASE + 16 * i + 16);
        }
        let n = holders.len();
        assert_eq!(n, SMALL as usize + 1);
        let allowed = allowed(n);
        assert!(allowed < 200, "{allowed}");

        // Across two small buffers above all the others: the big one.
        let (address, end) = (BASE + 16 * SMALL - 8, BASE + 16 * SMALL + 8);
        let found = holders.holder(address, end);
        assert_eq!(found.key, Some((BASE, 1)));
        assert!(found.looked_at <= allowed, "{}", found.looked_at);
        assert_eq!(walked(&all, address, end).key, found.key);
        assert!(walked(&all, address, end).looked_at > SMALL as usize / 2);

        // Past the big buffer's end, with all of them below: nothing.
        let (address, end) = (BASE + (64 << 20) - 8, BASE + (64 << 20) + 8);
        let found = holders.holder(address, end);
        assert_eq!(found.key, None);
        assert!(found.looked_at <= allowed, "{}", found.looked_at);
        assert_eq!(walked(&all, address, end).looked_at, n);

        // Every small buffer as itself, and the gap after each as the big
        // one; no lookup above the allowance.
        let mut most = 0;
        for i in 0..SMALL {
            let at = BASE + 16 * i;
            let own = holders.holder(at + 4, at + 12);
            assert_eq!(own.key, Some((at, 2 + i as usize)));
            let across = holders.holder(at + 12, at + 20);
            assert_eq!(across.key, Some((BASE, 1)));
            most = most.max(own.looked_at).max(across.looked_at);
        }
        assert!(most <= allowed, "{most} of {n} looked at");
    }

    /// Buffers of any size at any place, added and removed: every lookup
    /// gives what the walk gives, and looks at no more than allowed.
    #[test]
    fn lookups_agree_with_the_walk_for_seeded_buffers() {
        const BASE: u64 = 0x10_0000;
        const SPAN: u64 = 1 << 20;
        let mut seeded = Seeded(0x5eed_0bad_cafe);
        let mut holders = Holders::new();
        let mut all: BTreeMap<Key, u64> = BTreeMap::new();
        let mut most = 0;
        for round in 0..6000usize {
            let start = BASE + seeded.next(SPAN);
            // Mostly small, some covering much of the span, some to the
            // end of the address space.
            let end = match seeded.next(16) {
                0 => start + seeded.next(SPAN),
                1 => u64::MAX,
                _ => start + seeded.next(4096),
            };
            let key = (start, seeded.next(4) as usize);
            holders.insert(key, end);
            all.insert(key, end);
            if round % 3 == 2 {
                // One that is there (the first at or above a place), and
                // one that may not be.
                let from = (BASE + seeded.next(SPAN), 0);
                if let Some((&gone, _)) = all.range(from..).next() {
                    holders.remove(gone);
                    all.remove(&gone);
                }
                holders.remove((BASE + seeded.next(SPAN), 9));
            }
            assert_eq!(holders.len(), all.len());
            for _ in 0..8 {
                let address = BASE - 64 + seeded.next(SPAN + 128);
                let end = match seeded.next(8) {
                    0 => address + seeded.next(SPAN),
                    1 => u64::MAX,
                    _ => address + 1 + seeded.next(512),
                };
                let found = holders.holder(address, end);
                assert_eq!(
                    found.key,
                    walked(&all, address, end).key,
                    "[{address:#x}, {end:#x}) in round {round}"
                );
                assert!(
                    found.looked_at <= allowed(all.len()),
                    "{} of {} looked at in round {round}",
                    found.looked_at,
                    all.len()
                );
                most = most.max(found.looked_at);
            }
        }
        assert!(all.len() > 3000, "{}", all.len());
        assert!(most > 1);
        // The furthest ends are kept through every split and merge.
        fn checked(tree: &Tree) -> u64 {
            let Some(node) = tree else { return 0 };
            let expected = node.end.max(checked(&node.left)).max(checked(&node.right));
            assert_eq!(node.furthest, expected);
            expected
        }
        checked(&holders.root);
        assert!(depth(&holders.root) <= allowed(all.len()) / 2);
    }

    /// Keys in order, the worst case for a tree that does not balance.
    #[test]
    fn sorted_keys_do_not_make_a_deep_tree() {
        let mut holders = Holders::new();
        for i in 0..50_000u64 {
            holders.insert(
                (0x4000_0000 + 0x1_0000 * i, 5),
                0x4000_0000 + 0x1_0000 * (i + 1),
            );
        }
        assert_eq!(holders.len(), 50_000);
        let depth = depth(&holders.root);
        assert!(depth <= allowed(50_000) / 2, "depth {depth}");
    }
}
