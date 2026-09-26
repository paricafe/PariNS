//! Disposable derived data, never source authority. Explicit portable bytes;
//! decoding fills the final buffers directly, without mmap or a whole-file copy.
use super::super::canonical::{Builder, Limits, grow};
use super::{DERIVED_PREFIX, Index, NONE, Node};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

const MAX_DEPTH: usize = 255;
#[derive(Clone, Copy, Default)]
struct WalkFrame {
    node: usize,
    prefix_len: usize,
    next_child: u16,
    ancestor_suffixes: u8,
    entered: bool,
}
const SCRATCH_BYTES: usize = MAX_DEPTH * std::mem::size_of::<WalkFrame>() + 254;

fn source_ids_valid(ids: &[String]) -> bool {
    (1..=17).contains(&ids.len())
        && ids[0].is_empty()
        && ids[1..].iter().all(|id| {
            (1..=32).contains(&id.len())
                && id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        && ids[1..].windows(2).all(|pair| pair[0] < pair[1])
}

impl Index {
    pub(crate) fn publication_digest(&self, source_ids: &[String]) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"parins-policy-witness-v1\0");
        self.walk(source_ids.len(), &mut || Ok(()), |key, sources| {
            for (group, source) in sources.iter().enumerate() {
                if *source == NONE {
                    continue;
                }
                let id = &source_ids[usize::from(*source)];
                hash.update([group as u8]);
                hash.update((key.len() as u16).to_be_bytes());
                hash.update(key);
                hash.update([id.len() as u8]);
                hash.update(id.as_bytes());
            }
            Ok(())
        })
        .expect("built or fully validated immutable radix");
        hash.finalize().into()
    }
    pub(crate) fn write_to(
        &self,
        mut writer: impl Write,
        input_digest: [u8; 32],
        source_ids: &[String],
    ) -> Result<()> {
        ensure!(
            source_ids_valid(source_ids),
            "invalid derived index source IDs"
        );
        let mut hash = Sha256::new();
        let mut put = |bytes: &[u8]| -> Result<()> {
            writer.write_all(bytes)?;
            hash.update(bytes);
            Ok(())
        };
        put(DERIVED_PREFIX)?;
        put(&input_digest)?;
        put(&self.semantic_digest)?;
        for length in [
            self.nodes.len(),
            self.arena.len(),
            self.input_rules,
            self.index_rules,
        ] {
            put(&u32::try_from(length)?.to_be_bytes())?;
        }
        put(&(source_ids.len() as u16).to_be_bytes())?;
        for id in source_ids {
            put(&[id.len() as u8])?;
            put(id.as_bytes())?;
        }
        for node in &self.nodes {
            ensure!(
                node.sources
                    .iter()
                    .all(|s| *s == NONE || usize::from(*s) < source_ids.len()),
                "invalid derived index source slot"
            );
            let mut bytes = [0; 20];
            bytes[..4].copy_from_slice(&node.edge_offset.to_be_bytes());
            bytes[4..8].copy_from_slice(&node.first_child.to_be_bytes());
            bytes[8..10].copy_from_slice(&node.child_count.to_be_bytes());
            bytes[10] = node.edge_len;
            bytes[11] = node.first_byte;
            for (i, source) in node.sources.iter().enumerate() {
                bytes[12 + 2 * i..14 + 2 * i].copy_from_slice(&source.to_be_bytes());
            }
            put(&bytes)?;
        }
        put(&self.arena)?;
        writer.write_all(&hash.finalize())?;
        Ok(())
    }

    pub(crate) fn read_from(
        reader: impl Read,
        expected_input_digest: [u8; 32],
        expected_source_ids: &[String],
        limits: Limits,
    ) -> Result<Self> {
        Self::read_from_with_check(
            reader,
            expected_input_digest,
            expected_source_ids,
            limits,
            || Ok(()),
        )
    }

    /// The owner may enforce cancellation/deadline without relinquishing the
    /// worker's memory lease before these buffers and validation scratch die.
    pub(crate) fn read_from_with_check(
        mut reader: impl Read,
        expected_input_digest: [u8; 32],
        expected_source_ids: &[String],
        limits: Limits,
        mut check: impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        check()?;
        ensure!(
            source_ids_valid(expected_source_ids),
            "invalid derived index source IDs"
        );
        let mut hash = Sha256::new();
        let mut get = |bytes: &mut [u8]| -> Result<()> {
            reader
                .read_exact(bytes)
                .context("truncated derived index")?;
            hash.update(bytes);
            Ok(())
        };
        let mut header = [0; 98];
        get(&mut header)?;
        ensure!(
            &header[..16] == DERIVED_PREFIX,
            "unsupported derived index format"
        );
        ensure!(
            header[16..48] == expected_input_digest,
            "derived index input mismatch"
        );
        let semantic_digest = header[48..80].try_into().expect("fixed digest");
        let length =
            |offset| u32::from_be_bytes(header[offset..offset + 4].try_into().unwrap()) as usize;
        let node_count = length(80);
        let arena_len = length(84);
        let input_rules = length(88);
        let index_rules = length(92);
        ensure!(
            input_rules <= limits.max_rules && index_rules <= input_rules,
            "derived index rule limit"
        );
        ensure!(
            node_count > 0
                && node_count <= index_rules.saturating_mul(2).saturating_add(1)
                && arena_len <= node_count.saturating_mul(254),
            "derived index dimensions"
        );
        ensure!(
            u16::from_be_bytes(header[96..98].try_into().unwrap()) as usize
                == expected_source_ids.len(),
            "derived index sources mismatch"
        );
        for expected in expected_source_ids {
            let mut length = [0];
            get(&mut length)?;
            ensure!(
                usize::from(length[0]) == expected.len(),
                "derived index source mismatch"
            );
            let mut bytes = [0; 32];
            get(&mut bytes[..expected.len()])?;
            ensure!(
                &bytes[..expected.len()] == expected.as_bytes(),
                "derived index source mismatch"
            );
        }
        let mut budget = Builder::new(limits)?.budget;
        budget.reserve(SCRATCH_BYTES)?;
        let mut nodes = Vec::new();
        let mut arena = Vec::new();
        grow(&mut nodes, node_count, &mut budget)?;
        grow(&mut arena, arena_len, &mut budget)?;
        for i in 0..node_count {
            if i.is_multiple_of(4096) {
                check()?;
            }
            let mut bytes = [0; 20];
            get(&mut bytes)?;
            nodes.push(Node {
                edge_offset: u32::from_be_bytes(bytes[..4].try_into().unwrap()),
                first_child: u32::from_be_bytes(bytes[4..8].try_into().unwrap()),
                child_count: u16::from_be_bytes(bytes[8..10].try_into().unwrap()),
                edge_len: bytes[10],
                first_byte: bytes[11],
                sources: std::array::from_fn(|i| {
                    u16::from_be_bytes(bytes[12 + 2 * i..14 + 2 * i].try_into().unwrap())
                }),
            });
        }
        arena.resize(arena_len, 0);
        for chunk in arena.chunks_mut(8192) {
            check()?;
            get(chunk)?;
        }
        let mut trailer = [0; 32];
        reader.read_exact(&mut trailer)?;
        ensure!(
            <[u8; 32]>::from(hash.finalize()) == trailer,
            "derived index checksum mismatch"
        );
        let mut extra = [0];
        ensure!(
            reader.read(&mut extra)? == 0,
            "trailing derived index bytes"
        );
        let result = Self {
            nodes,
            arena,
            semantic_digest,
            input_rules,
            index_rules,
            peak_bytes: budget.peak,
        };
        let mut counts = [0usize; 4];
        result.walk(expected_source_ids.len(), &mut check, |_, sources| {
            for (group, source) in sources.iter().enumerate() {
                counts[group] += usize::from(*source != NONE);
            }
            Ok(())
        })?;
        ensure!(
            counts.iter().sum::<usize>() == index_rules,
            "derived index terminal count"
        );
        let mut semantic = Sha256::new();
        semantic.update(b"parins-policy-semantics-v1\0");
        for (group, count) in counts.into_iter().enumerate() {
            semantic.update([group as u8]);
            semantic.update((count as u64).to_be_bytes());
            result.walk(expected_source_ids.len(), &mut check, |key, sources| {
                if sources[group] != NONE {
                    semantic.update((key.len() as u16).to_be_bytes());
                    semantic.update(key);
                }
                Ok(())
            })?;
        }
        ensure!(
            <[u8; 32]>::from(semantic.finalize()) == semantic_digest,
            "derived index semantic mismatch"
        );
        Ok(result)
    }

    /// Iterative lexicographic walk also verifies canonical allocation order:
    /// every node/edge occurs once, with no cycles, aliases, gaps or dead leaves.
    fn walk(
        &self,
        source_count: usize,
        check: &mut impl FnMut() -> Result<()>,
        mut visit: impl FnMut(&[u8], &[u16; 4]) -> Result<()>,
    ) -> Result<()> {
        let root = self.nodes[0];
        ensure!(
            root.edge_offset == 0
                && root.edge_len == 0
                && root.first_byte == 0
                && root.sources == [NONE; 4],
            "invalid derived index root"
        );
        let mut frames = [WalkFrame::default(); MAX_DEPTH];
        let mut depth = 1;
        let mut key = [0; 254];
        let mut next_allocation = 1usize;
        let mut next_edge = 0usize;
        let mut visited = 0usize;
        while depth > 0 {
            let frame = &mut frames[depth - 1];
            let node = self.nodes[frame.node];
            let end = frame.prefix_len + usize::from(node.edge_len);
            if !frame.entered {
                if visited.is_multiple_of(4096) {
                    check()?;
                }
                visited += 1;
                ensure!(end <= key.len(), "derived index DNS key length");
                if frame.node != 0 {
                    ensure!(
                        node.edge_len != 0 && node.edge_offset as usize == next_edge,
                        "derived index edge layout"
                    );
                    let edge_end = next_edge
                        .checked_add(usize::from(node.edge_len))
                        .context("derived index edge overflow")?;
                    let edge = self
                        .arena
                        .get(next_edge..edge_end)
                        .context("derived index edge bounds")?;
                    ensure!(
                        edge[0] == node.first_byte,
                        "derived index edge ordering byte"
                    );
                    key[frame.prefix_len..end].copy_from_slice(edge);
                    next_edge = edge_end;
                }
                ensure!(
                    node.first_child as usize == next_allocation && node.child_count <= 256,
                    "derived index graph allocation"
                );
                next_allocation = next_allocation
                    .checked_add(usize::from(node.child_count))
                    .context("derived index child overflow")?;
                let children = self
                    .nodes
                    .get(node.first_child as usize..next_allocation)
                    .context("derived index child bounds")?;
                ensure!(
                    children
                        .windows(2)
                        .all(|pair| pair[0].first_byte < pair[1].first_byte),
                    "derived index children unsorted"
                );
                let terminal = node.sources.iter().any(|s| *s != NONE);
                ensure!(
                    frame.node == 0 || terminal || node.child_count >= 2,
                    "derived index uncompressed dead path"
                );
                if terminal {
                    validate_key(&key[..end])?;
                }
                for (group, source) in node.sources.iter().enumerate() {
                    ensure!(
                        *source == NONE || usize::from(*source) < source_count,
                        "derived index source slot"
                    );
                    if *source != NONE {
                        ensure!(
                            frame.ancestor_suffixes & (1 << (group / 2)) == 0,
                            "derived index redundant suffix child"
                        );
                        if group % 2 == 1 {
                            ensure!(
                                node.sources[group - 1] == NONE,
                                "derived index redundant exact"
                            );
                        }
                    }
                }
                visit(&key[..end], &node.sources)?;
                frame.entered = true;
            }
            if frame.next_child == node.child_count {
                depth -= 1;
                continue;
            }
            let child = node.first_child as usize + usize::from(frame.next_child);
            frame.next_child += 1;
            let ancestor_suffixes = frame.ancestor_suffixes
                | u8::from(node.sources[0] != NONE)
                | (u8::from(node.sources[2] != NONE) << 1);
            ensure!(depth < MAX_DEPTH, "derived index graph depth");
            frames[depth] = WalkFrame {
                node: child,
                prefix_len: end,
                ancestor_suffixes,
                ..WalkFrame::default()
            };
            depth += 1;
        }
        ensure!(
            visited == self.nodes.len()
                && next_allocation == self.nodes.len()
                && next_edge == self.arena.len(),
            "derived index unreachable data"
        );
        Ok(())
    }
}

fn validate_key(mut key: &[u8]) -> Result<()> {
    ensure!(!key.is_empty(), "derived index empty rule");
    while let Some((&length, rest)) = key.split_first() {
        ensure!((1..=63).contains(&length), "derived index label length");
        let label = rest
            .get(..usize::from(length))
            .context("derived index label bounds")?;
        ensure!(
            label
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_'),
            "derived index label encoding"
        );
        key = &rest[usize::from(length)..];
    }
    Ok(())
}
