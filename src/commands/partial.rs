//! Bounded line edits from the repo version to the home version.

use serde::Serialize;
use std::collections::BTreeSet;

const MAX_BYTES: usize = 256 * 1024;
const MAX_CELLS: usize = 1_000_000;
const MAX_WORK: usize = 16_000_000;
const MAX_EDITS: u32 = 1024;

#[derive(Debug, Serialize)]
pub struct LineEdit {
    id: u32,
    old_line: Option<u32>,
    new_line: Option<u32>,
    before: Option<String>,
    after: Option<String>,
}

enum Part {
    Same(String),
    Change(LineEdit),
}

pub struct TextChanges {
    parts: Vec<Part>,
}

fn lines(bytes: &[u8]) -> Option<Vec<&str>> {
    if bytes.len() > MAX_BYTES {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return None;
    }
    Some(text.split_inclusive('\n').collect())
}

impl TextChanges {
    pub(crate) fn between(repo: &[u8], home: &[u8]) -> Option<Self> {
        let old = lines(repo)?;
        let new = lines(home)?;
        let width = new.len().checked_add(1)?;
        let cells = old.len().checked_add(1)?.checked_mul(width)?;
        let longest = old.iter().chain(&new).map(|s| s.len()).max().unwrap_or(0);
        if cells > MAX_CELLS || cells.checked_mul(longest.max(1))? > MAX_WORK {
            return None;
        }
        let mut table = vec![0_u32; cells];
        for i in (0..old.len()).rev() {
            for j in (0..new.len()).rev() {
                let pos = i.checked_mul(width)?.checked_add(j)?;
                let down = pos.checked_add(width)?;
                let right = pos.checked_add(1)?;
                let value = if old.get(i)? == new.get(j)? {
                    table.get(down.checked_add(1)?)?.checked_add(1)?
                } else {
                    (*table.get(down)?).max(*table.get(right)?)
                };
                *table.get_mut(pos)? = value;
            }
        }
        let (mut i, mut j) = (0_usize, 0_usize);
        let mut parts = Vec::new();
        let mut removed = Vec::new();
        let mut added = Vec::new();
        let mut id = 0_u32;
        while i < old.len() || j < new.len() {
            if i < old.len() && j < new.len() && old.get(i)? == new.get(j)? {
                flush(&mut parts, &mut removed, &mut added, &mut id)?;
                parts.push(Part::Same((*old.get(i)?).to_owned()));
                i = i.checked_add(1)?;
                j = j.checked_add(1)?;
            } else {
                let pos = i.checked_mul(width)?.checked_add(j)?;
                if i < old.len()
                    && (j == new.len()
                        || table.get(pos.checked_add(width)?)? >= table.get(pos.checked_add(1)?)?)
                {
                    removed.push((
                        u32::try_from(i.checked_add(1)?).ok()?,
                        (*old.get(i)?).to_owned(),
                    ));
                    i = i.checked_add(1)?;
                } else {
                    added.push((
                        u32::try_from(j.checked_add(1)?).ok()?,
                        (*new.get(j)?).to_owned(),
                    ));
                    j = j.checked_add(1)?;
                }
            }
        }
        flush(&mut parts, &mut removed, &mut added, &mut id)?;
        Some(Self { parts })
    }

    #[cfg(test)]
    fn edits(&self) -> Vec<&LineEdit> {
        self.parts
            .iter()
            .filter_map(|part| match part {
                Part::Change(edit) => Some(edit),
                Part::Same(_) => None,
            })
            .collect()
    }

    pub(crate) fn into_edits(self) -> Vec<LineEdit> {
        self.parts
            .into_iter()
            .filter_map(|part| match part {
                Part::Change(edit) => Some(edit),
                Part::Same(_) => None,
            })
            .collect()
    }

    pub(crate) fn project(&self, ids: &[u32]) -> Option<Vec<u8>> {
        let mut selected: BTreeSet<u32> = ids.iter().copied().collect();
        if selected.len() != ids.len() {
            return None;
        }
        let mut result = Vec::new();
        for part in &self.parts {
            let text = match part {
                Part::Same(text) => Some(text),
                Part::Change(edit) if selected.remove(&edit.id) => edit.after.as_ref(),
                Part::Change(edit) => edit.before.as_ref(),
            };
            if let Some(text) = text {
                // An insertion after an unterminated line also needs its newline edit.
                if !result.is_empty() && !result.ends_with(b"\n") {
                    return None;
                }
                result.extend_from_slice(text.as_bytes());
            }
        }
        selected.is_empty().then_some(result)
    }
}

fn flush(
    parts: &mut Vec<Part>,
    removed: &mut Vec<(u32, String)>,
    added: &mut Vec<(u32, String)>,
    id: &mut u32,
) -> Option<()> {
    let mut old = removed.drain(..);
    let mut new = added.drain(..);
    loop {
        let before = old.next();
        let after = new.next();
        if before.is_none() && after.is_none() {
            break;
        }
        *id = id.checked_add(1)?;
        if *id > MAX_EDITS {
            return None;
        }
        let (old_line, before) = before.map_or((None, None), |(n, s)| (Some(n), Some(s)));
        let (new_line, after) = after.map_or((None, None), |(n, s)| (Some(n), Some(s)));
        parts.push(Part::Change(LineEdit {
            id: *id,
            old_line,
            new_line,
            before,
            after,
        }));
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projections_preserve_lines_and_order() {
        let old = b"same\nold1\nold2\nrepeat\nrepeat\nend";
        let new = b"same\nnew1\nnew2\nrepeat\ninsert\nrepeat\nend\r\n";
        let changes = TextChanges::between(old, new).unwrap();
        let ids: Vec<_> = changes.edits().iter().map(|e| e.id).collect();
        assert_eq!(changes.project(&[]).as_deref(), Some(old.as_slice()));
        assert_eq!(changes.project(&ids).as_deref(), Some(new.as_slice()));
        assert_eq!(
            changes.project(&[2, 1]).as_deref(),
            Some(b"same\nnew1\nnew2\nrepeat\nrepeat\nend".as_slice())
        );
        assert!(changes.project(&[0]).is_none());
        assert!(changes.project(&[1, 1]).is_none());
        let deletion = TextChanges::between(b"a\nb\n", b"a\n").unwrap();
        assert_eq!(deletion.project(&[1]).as_deref(), Some(b"a\n".as_slice()));
    }

    #[test]
    fn insertion_after_an_unterminated_line_requires_the_newline_edit() {
        let changes = TextChanges::between(b"last", b"last\nnext").unwrap();
        assert!(changes.project(&[2]).is_none());
        assert_eq!(
            changes.project(&[1, 2]).as_deref(),
            Some(b"last\nnext".as_slice())
        );
    }

    #[test]
    fn refuses_binary_controls_and_over_budget() {
        assert!(TextChanges::between(b"a\0", b"b").is_none());
        assert!(TextChanges::between(b"\xff", b"b").is_none());
        assert!(TextChanges::between(&vec![b'a'; MAX_BYTES.saturating_add(1)], b"").is_none());
        let many = "a\n".repeat(1000);
        assert!(TextChanges::between(many.as_bytes(), many.as_bytes()).is_none());
        assert!(TextChanges::between(b"", "x\n".repeat(1025).as_bytes()).is_none());
    }
}
