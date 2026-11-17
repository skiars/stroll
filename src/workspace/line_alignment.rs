use gpui_component::input::{Rope, RopeExt};
use similar::{DiffTag, TextDiff};
use std::{
    collections::HashMap,
    ops::{Range, RangeInclusive},
    time::{Duration, Instant},
};

pub(super) fn equal_line_ranges(
    original: &[&str],
    current: &[&str],
    deadline: Instant,
) -> Vec<(Range<usize>, Range<usize>)> {
    TextDiff::configure()
        .deadline(deadline)
        .diff_slices(original, current)
        .ops()
        .iter()
        .filter(|op| op.tag() == DiffTag::Equal)
        .map(|op| (op.old_range(), op.new_range()))
        .collect()
}

const MAX_BYTES: usize = 1024 * 1024;
const MAX_LINES: usize = 32_768;
const MAX_CONTEXT_LINES: usize = 5;
const ALIGNMENT_BUDGET: Duration = Duration::from_millis(100);

pub(super) enum LineAlignment {
    Identity,
    Partial(Vec<AlignedLines>),
}

pub(super) struct AlignedLines {
    original: usize,
    current: usize,
    len: usize,
}

#[derive(Clone, Copy, Default)]
enum Match {
    #[default]
    None,
    Unique(usize),
    Ambiguous,
}

impl Match {
    fn add(&mut self, row: usize) {
        *self = match *self {
            Self::None => Self::Unique(row),
            Self::Unique(previous) if previous == row => *self,
            _ => Self::Ambiguous,
        };
    }
}

impl LineAlignment {
    pub(super) fn between(original: &Rope, current: &Rope) -> Option<Self> {
        if original == current {
            return Some(Self::Identity);
        }
        if original.len().max(current.len()) > MAX_BYTES
            || original.lines_len().max(current.lines_len()) > MAX_LINES
        {
            return None;
        }
        Self::partial(original.to_string(), current.to_string())
    }

    fn partial(original: String, current: String) -> Option<Self> {
        let deadline = Instant::now() + ALIGNMENT_BUDGET;
        // Use the editor's LF coordinates, including a final line without a terminator.
        let original = original.split_terminator('\n').collect::<Vec<_>>();
        let current = current.split_terminator('\n').collect::<Vec<_>>();
        let equal = equal_line_ranges(&original, &current, deadline);
        if equal.is_empty() || Instant::now() >= deadline {
            return None;
        }

        // Intern each line once so that indexing context windows hashes only small integers.
        let mut ids = HashMap::new();
        let [original, current] = [original, current].map(|lines| {
            lines
                .into_iter()
                .map(|line| {
                    let next = ids.len();
                    *ids.entry(line).or_insert(next)
                })
                .collect::<Vec<_>>()
        });
        let mut to_current = vec![Match::None; original.len()];
        let mut to_original = vec![Match::None; current.len()];
        for width in 1..=MAX_CONTEXT_LINES.min(original.len()).min(current.len()) {
            if Instant::now() >= deadline {
                return None;
            }
            let mut contexts = HashMap::<&[usize], (Match, Match)>::new();
            for (row, context) in original.windows(width).enumerate() {
                contexts.entry(context).or_default().0.add(row);
            }
            for (row, context) in current.windows(width).enumerate() {
                if let Some((_, target)) = contexts.get_mut(context) {
                    target.add(row);
                }
            }
            for (from, to) in contexts.into_values() {
                if let (Match::Unique(from), Match::Unique(to)) = (from, to) {
                    for offset in 0..width {
                        to_current[from + offset].add(to + offset);
                        to_original[to + offset].add(from + offset);
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return None;
        }

        // The diff proposes a monotonic alignment. Unique contexts must agree with it,
        // including when different contexts suggest different copies of a repeated line.
        let mut blocks: Vec<AlignedLines> = Vec::new();
        for (original, current) in equal {
            for (original, current) in original.zip(current) {
                if !matches!(to_current[original], Match::Unique(row) if row == current)
                    || !matches!(to_original[current], Match::Unique(row) if row == original)
                {
                    continue;
                }
                if let Some(block) = blocks.last_mut().filter(|block| {
                    block.original + block.len == original && block.current + block.len == current
                }) {
                    block.len += 1;
                } else {
                    blocks.push(AlignedLines {
                        original,
                        current,
                        len: 1,
                    });
                }
            }
        }
        Some(Self::Partial(blocks))
    }

    pub(super) fn current_row(&self, original: usize) -> Option<usize> {
        match self {
            Self::Identity => Some(original),
            Self::Partial(blocks) => {
                let index = blocks.partition_point(|block| block.original <= original);
                let block = blocks.get(index.checked_sub(1)?)?;
                let offset = original - block.original;
                (offset < block.len).then(|| block.current + offset)
            }
        }
    }

    pub(super) fn original_rows(
        &self,
        current: RangeInclusive<usize>,
    ) -> Option<RangeInclusive<usize>> {
        match self {
            Self::Identity => Some(current),
            Self::Partial(blocks) => {
                let start = *current.start();
                let end = *current.end();
                let index = blocks.partition_point(|block| block.current <= start);
                let block = blocks.get(index.checked_sub(1)?)?;
                (end < block.current + block.len).then(|| {
                    block.original + start - block.current..=block.original + end - block.current
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn align(original: &str, current: &str) -> LineAlignment {
        LineAlignment::between(&Rope::from(original), &Rope::from(current)).unwrap()
    }

    #[test]
    fn maps_lines_with_unique_context_and_checks_contiguous_ranges() {
        let alignment = align(
            "first\nrepeat\nremove\nsecond\nrepeat",
            "insert\nfirst\nrepeat\nsecond\nrepeat\n",
        );
        assert_eq!(alignment.current_row(1), Some(2));
        assert_eq!(alignment.current_row(2), None);
        assert_eq!(alignment.current_row(4), Some(4));
        assert_eq!(alignment.original_rows(3..=4), Some(3..=4));
        assert_eq!(alignment.original_rows(2..=3), None);
    }

    #[test]
    fn rejects_conflicting_contexts_around_repeated_code() {
        let alignment = align("header\ncall\nend\n", "header\ncall\ncall\nend\n");
        assert_eq!(alignment.current_row(1), None);
        assert_eq!(alignment.current_row(0), Some(0));
        assert_eq!(alignment.current_row(2), Some(3));
        let alignment = align("header\ncall\ncall\nend\n", "header\ncall\nend\n");
        assert_eq!(alignment.current_row(1), None);
        assert_eq!(alignment.current_row(2), None);
        assert_eq!(alignment.original_rows(1..=1), None);
    }

    #[test]
    fn bounds_partial_alignment_without_limiting_identical_snapshots() {
        let original = Rope::from("line\n".repeat(MAX_LINES));
        let current = Rope::from(format!("insert\n{original}"));
        assert!(LineAlignment::between(&original, &current).is_none());
        assert!(matches!(
            LineAlignment::between(&original, &original),
            Some(LineAlignment::Identity)
        ));
    }
}
