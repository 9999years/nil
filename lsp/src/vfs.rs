use lsp_types::Url;
use nil::{Change, FileId, FileSet, SourceRoot, VfsPath};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::{fmt, mem};
use text_size::TextSize;

pub struct Vfs {
    // FIXME: Currently this list is append-only.
    files: Vec<Option<(Arc<str>, LineMap)>>,
    local_root: PathBuf,
    local_file_set: FileSet,
    root_changed: bool,
    change: Change,
}

impl fmt::Debug for Vfs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vfs")
            .field("file_cnt", &self.files.len())
            .field("local_root", &self.local_root)
            .finish_non_exhaustive()
    }
}

impl Vfs {
    pub fn new(local_root: PathBuf) -> Self {
        Self {
            files: Vec::new(),
            local_root,
            local_file_set: FileSet::default(),
            root_changed: false,
            change: Change::default(),
        }
    }

    fn alloc_file_id(&mut self) -> FileId {
        let id = u32::try_from(self.files.len()).expect("Length overflow");
        self.files.push(None);
        FileId(id)
    }

    fn uri_to_vpath(&self, uri: &Url) -> Option<VfsPath> {
        let path = uri.to_file_path().ok()?;
        let relative_path = path.strip_prefix(&self.local_root).ok()?;
        VfsPath::from_path(relative_path)
    }

    pub fn set_uri_content(&mut self, uri: &Url, text: Option<String>) -> Option<FileId> {
        let vpath = self.uri_to_vpath(uri)?;
        let content = text.and_then(LineMap::normalize);
        let (file, (text, line_map)) =
            match (self.local_file_set.get_file_for_path(&vpath), content) {
                (Some(file), None) => {
                    self.local_file_set.remove_file(file);
                    self.root_changed = true;
                    self.files[file.0 as usize] = None;
                    return None;
                }
                (None, None) => return None,
                (Some(file), Some(content)) => (file, content),
                (None, Some(content)) => {
                    let file = self.alloc_file_id();
                    self.local_file_set.insert(file, vpath);
                    self.root_changed = true;
                    (file, content)
                }
            };
        let text = <Arc<str>>::from(text);
        self.change.change_file(file, Some(text.clone()));
        self.files[file.0 as usize] = Some((text, line_map));
        Some(file)
    }

    pub fn get_file_for_uri(&self, uri: &Url) -> Option<FileId> {
        let vpath = self.uri_to_vpath(uri)?;
        self.local_file_set.get_file_for_path(&vpath)
    }

    pub fn get_uri_for_file(&self, file: FileId) -> Option<Url> {
        let vpath = self.local_file_set.get_path_for_file(file)?.as_str();
        assert!(!vpath.is_empty(), "Root is a directory");
        let path = self.local_root.join(vpath.strip_prefix('/')?);
        Url::from_file_path(path).ok()
    }

    pub fn take_change(&mut self) -> Change {
        let mut change = mem::take(&mut self.change);
        if self.root_changed {
            self.root_changed = false;
            change.set_roots(vec![SourceRoot::new_local(self.local_file_set.clone())]);
        }
        change
    }

    pub fn get_line_map(&self, file_id: FileId) -> Option<&LineMap> {
        Some(&self.files.get(file_id.0 as usize)?.as_ref()?.1)
    }
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct LineMap {
    line_starts: Vec<u32>,
    char_diffs: HashMap<u32, Vec<(u32, CodeUnitsDiff)>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeUnitsDiff {
    One = 1,
    Two = 2,
}

impl LineMap {
    fn normalize(text: String) -> Option<(String, Self)> {
        // Too large for `TextSize`.
        if text.len() > u32::MAX as usize {
            return None;
        }

        let text = text.replace('\r', "");
        let bytes = text.as_bytes();

        let mut line_starts = Some(0)
            .into_iter()
            .chain(
                bytes
                    .iter()
                    .zip(0u32..)
                    .filter(|(b, _)| **b == b'\n')
                    .map(|(_, i)| i + 1),
            )
            .collect::<Vec<_>>();
        line_starts.push(text.len() as u32);

        let mut char_diffs = HashMap::new();
        for ((&start, &end), i) in line_starts.iter().zip(&line_starts[1..]).zip(0u32..) {
            let mut diffs = Vec::new();
            for (&b, pos) in bytes[start as usize..end as usize].iter().zip(0u32..) {
                let diff = match b {
                    0b0000_0000..=0b0111_1111 |                      // utf8_len == 1, utf16_len == 1
                    0b1000_0000..=0b1011_1111 => continue,           // Continuation bytes.
                    0b1100_0000..=0b1101_1111 => CodeUnitsDiff::One, // utf8_len == 2, utf16_len == 1
                    0b1110_0000..=0b1110_1111 => CodeUnitsDiff::Two, // utf8_len == 3, utf16_len == 1
                    0b1111_0000.. => CodeUnitsDiff::Two,             // utf8_len == 4, utf16_len == 2
                };
                diffs.push((pos, diff));
            }
            if !diffs.is_empty() {
                char_diffs.insert(i, diffs);
            }
        }

        let this = Self {
            line_starts,
            char_diffs,
        };
        Some((text, this))
    }

    pub fn pos(&self, line: u32, mut col: u32) -> TextSize {
        let pos = self.line_starts.get(line as usize).copied().unwrap_or(0);
        if let Some(diffs) = self.char_diffs.get(&line) {
            for &(char_pos, diff) in diffs {
                if char_pos < col {
                    col += diff as u32;
                }
            }
        }
        (pos + col).into()
    }

    pub fn line_col(&self, pos: TextSize) -> (u32, u32) {
        let pos = u32::from(pos);
        let line = self
            .line_starts
            .partition_point(|&i| i <= pos)
            .saturating_sub(1);
        let mut col = pos - self.line_starts[line];
        if let Some(diffs) = self.char_diffs.get(&(line as u32)) {
            col -= diffs
                .iter()
                .take_while(|(char_pos, _)| *char_pos < col)
                .map(|(_, diff)| *diff as u32)
                .sum::<u32>();
        }
        (line as u32, col)
    }
}

#[cfg(test)]
mod tests {
    use super::{CodeUnitsDiff, LineMap};
    use std::collections::HashMap;

    #[test]
    fn line_map_ascii() {
        let (s, map) = LineMap::normalize("hello\nworld\nend".into()).unwrap();
        assert_eq!(s, "hello\nworld\nend");
        assert_eq!(&map.line_starts, &[0, 6, 12, 15]);

        let mapping = [
            (0, 0, 0),
            (2, 0, 2),
            (5, 0, 5),
            (6, 1, 0),
            (11, 1, 5),
            (12, 2, 0),
        ];
        for (pos, line, col) in mapping {
            assert_eq!(map.line_col(pos.into()), (line, col));
            assert_eq!(map.pos(line, col), pos.into());
        }
    }

    #[test]
    fn line_map_unicode() {
        let (s, map) = LineMap::normalize("_A_ß_ℝ_💣_".into()).unwrap();
        assert_eq!(s, "_A_ß_ℝ_💣_");
        assert_eq!(&map.line_starts, &[0, 15]);
        assert_eq!(
            &map.char_diffs,
            &HashMap::from([(
                0u32,
                vec![
                    (3u32, CodeUnitsDiff::One),
                    (6, CodeUnitsDiff::Two),
                    (10, CodeUnitsDiff::Two),
                ],
            )])
        );

        let mapping = [
            (0, 0, 0),
            (1, 0, 1),
            (2, 0, 2),
            (3, 0, 3),
            (5, 0, 4),
            (6, 0, 5),
            (9, 0, 6),
            (10, 0, 7),
            (14, 0, 9),
        ];
        for (pos, line, col) in mapping {
            assert_eq!(map.line_col(pos.into()), (line, col));
            assert_eq!(map.pos(line, col), pos.into());
        }
    }
}
