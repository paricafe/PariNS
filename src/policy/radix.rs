//! Immutable, bounded compressed byte-radix filtering index.
//! Development integration does not imply performance release acceptance.
use super::canonical::{Canonical, Error, Group, Key, grow, key};
use hickory_proto::rr::Name;
use std::mem::size_of;

const NONE: u16 = u16::MAX;
/// Exact ownership prefix: magic, big-endian index version, semantics version.
/// The 32-byte input digest follows immediately, starting at byte 16.
pub(crate) const DERIVED_PREFIX: &[u8; 16] = b"PRNSRDX\0\0\0\0\x01\0\0\0\x01";
// Every non-root edge consumes at least one of at most 254 encoded bytes.
// Explicit frames replace recursion so variable-depth build scratch is charged.
const MAX_BUILD_DEPTH: usize = 255;
#[derive(Clone, Copy)]
struct Frame {
    parent: usize,
    start: usize,
    end: usize,
    depth: usize,
    next_child: usize,
}
impl Frame {
    fn new(parent: usize, start: usize, end: usize, depth: usize) -> Self {
        Self {
            parent,
            start,
            end,
            depth,
            next_child: usize::MAX,
        }
    }
}
const BUILD_SCRATCH_BYTES: usize = MAX_BUILD_DEPTH * size_of::<Frame>();
#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct Node {
    edge_offset: u32,
    first_child: u32,
    child_count: u16,
    edge_len: u8,
    first_byte: u8,
    sources: [u16; 4],
}
const _: () = assert!(size_of::<Node>() == 20);
impl Node {
    fn empty() -> Self {
        Self {
            edge_offset: 0,
            first_child: 0,
            child_count: 0,
            edge_len: 0,
            first_byte: 0,
            sources: [NONE; 4],
        }
    }
}

#[derive(Debug)]
pub struct Index {
    nodes: Vec<Node>,
    arena: Vec<u8>,
    pub semantic_digest: [u8; 32],
    pub input_rules: usize,
    pub index_rules: usize,
    pub peak_bytes: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub group: Group,
    pub source_slot: u16,
    pub matched_key_len: u16,
}

impl Index {
    pub(super) fn build(input: Canonical) -> Result<Self, Error> {
        Self::build_with_check(input, || Ok(()))
    }
    pub(super) fn build_with_check(
        mut input: Canonical,
        mut check: impl FnMut() -> Result<(), Error>,
    ) -> Result<Self, Error> {
        check()?;
        let index_rules = input.records.len();
        input.records.sort_unstable_by(|a, b| {
            key(&input.arena, a.entry)
                .cmp(key(&input.arena, b.entry))
                .then((a.group as u8).cmp(&(b.group as u8)))
        });
        check()?;
        let mut result = Self {
            nodes: Vec::new(),
            arena: Vec::new(),
            semantic_digest: input.semantic_digest,
            input_rules: input.input_rules,
            index_rules,
            peak_bytes: 0,
        };
        grow(&mut result.nodes, 1, &mut input.budget)?;
        result.nodes.push(Node::empty());
        result.children(&mut input, &mut check)?;
        result.peak_bytes = input.budget.peak;
        Ok(result)
    }
    fn children(
        &mut self,
        input: &mut Canonical,
        check: &mut impl FnMut() -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut frames = Vec::new();
        grow(&mut frames, MAX_BUILD_DEPTH, &mut input.budget)?;
        debug_assert!(frames.capacity() * size_of::<Frame>() >= BUILD_SCRATCH_BYTES);
        frames.push(Frame::new(0, 0, input.records.len(), 0));
        let mut processed = 0usize;
        while let Some(frame) = frames.last_mut() {
            processed += 1;
            if processed.is_multiple_of(4096) {
                check()?;
            }
            if frame.next_child == usize::MAX {
                while frame.start < frame.end
                    && usize::from(input.records[frame.start].entry.len) == frame.depth
                {
                    let record = input.records[frame.start];
                    self.nodes[frame.parent].sources[record.group as usize] =
                        record.entry.source_slot;
                    frame.start += 1;
                }
                let mut count = 0usize;
                let mut position = frame.start;
                while position < frame.end {
                    count += 1;
                    let byte = key(&input.arena, input.records[position].entry)[frame.depth];
                    position += 1;
                    while position < frame.end
                        && key(&input.arena, input.records[position].entry)[frame.depth] == byte
                    {
                        position += 1;
                        if position.is_multiple_of(4096) {
                            check()?;
                        }
                    }
                }
                let first_child = self.nodes.len();
                grow(&mut self.nodes, first_child + count, &mut input.budget)?;
                self.nodes.resize(first_child + count, Node::empty());
                self.nodes[frame.parent].first_child = first_child as u32;
                self.nodes[frame.parent].child_count = count as u16;
                frame.next_child = first_child;
            }
            if frame.start == frame.end {
                frames.pop();
                continue;
            }
            let Frame {
                start,
                end,
                depth,
                next_child: child,
                ..
            } = *frame;
            let first = key(&input.arena, input.records[start].entry);
            let byte = first[depth];
            let mut stop = start + 1;
            while stop < end && key(&input.arena, input.records[stop].entry)[depth] == byte {
                stop += 1;
                if stop.is_multiple_of(4096) {
                    check()?;
                }
            }
            let last = key(&input.arena, input.records[stop - 1].entry);
            let mut common = depth + 1;
            while common < first.len().min(last.len()) && first[common] == last[common] {
                common += 1;
            }
            // The shortest key in this lexicographic range bounds the common
            // prefix, so a complete-rule terminal is never compressed across.
            let offset = self.arena.len();
            grow(&mut self.arena, offset + common - depth, &mut input.budget)?;
            self.arena.extend_from_slice(&first[depth..common]);
            self.nodes[child] = Node {
                edge_offset: offset as u32,
                edge_len: (common - depth) as u8,
                first_byte: byte,
                ..Node::empty()
            };
            frame.start = stop;
            frame.next_child += 1;
            assert!(
                frames.len() < MAX_BUILD_DEPTH,
                "nonempty edges bound radix depth"
            );
            frames.push(Frame::new(child, start, stop, common));
        }
        input.budget.release(frames.capacity() * size_of::<Frame>());
        check()?;
        Ok(())
    }
    pub fn lookup(&self, name: &Name) -> Option<Match> {
        let query = Key::query(name);
        let query = query.as_slice();
        let mut index = 0;
        let mut position = 0;
        let mut blocked = None;
        loop {
            let node = self.nodes[index];
            let found = |group: Group| {
                (node.sources[group as usize] != NONE).then_some(Match {
                    group,
                    source_slot: node.sources[group as usize],
                    matched_key_len: position as u16,
                })
            };
            if let Some(allow) = found(Group::AllowSuffix) {
                return Some(allow);
            }
            if position == query.len() {
                if let Some(allow) = found(Group::AllowExact) {
                    return Some(allow);
                }
                return blocked
                    .or_else(|| found(Group::BlockSuffix))
                    .or_else(|| found(Group::BlockExact));
            }
            blocked = blocked.or_else(|| found(Group::BlockSuffix));
            let children = &self.nodes[node.first_child as usize
                ..node.first_child as usize + usize::from(node.child_count)];
            let Ok(child) = children.binary_search_by_key(&query[position], |node| node.first_byte)
            else {
                return blocked;
            };
            index = node.first_child as usize + child;
            let next = self.nodes[index];
            let edge = &self.arena
                [next.edge_offset as usize..next.edge_offset as usize + usize::from(next.edge_len)];
            if !query[position..].starts_with(edge) {
                return blocked;
            }
            position += edge.len();
        }
    }
    pub fn index_bytes(&self) -> usize {
        self.arena.capacity() + self.nodes.capacity() * size_of::<Node>()
    }
    #[allow(dead_code)] // Standalone benchmark also compiles this source without cfg(test).
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    /// Only copies the bounded matched rule, never the whole source.
    pub fn rule(&self, name: &Name, matched: Match) -> String {
        let key = Key::query(name);
        let mut bytes = &key.as_slice()[..usize::from(matched.matched_key_len)];
        let mut labels = [&[][..]; 127];
        let mut count = 0;
        while let Some((&len, rest)) = bytes.split_first() {
            labels[count] = &rest[..usize::from(len)];
            count += 1;
            bytes = &rest[usize::from(len)..];
        }
        let mut result = String::with_capacity(253);
        for label in labels[..count].iter().rev() {
            if !result.is_empty() {
                result.push('.');
            }
            result.push_str(std::str::from_utf8(label).expect("matched rules are ASCII"));
        }
        result
    }
}

#[cfg(test)]
#[path = "radix_tests.rs"]
mod tests;

#[path = "radix_disk.rs"]
mod disk;
