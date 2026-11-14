//! Prompt editing and workspace file mentions: the input buffer, the
//! `@path` completion index, and expansion of mentioned files into attachments.

use std::path::Path;

#[derive(Debug, Default)]
pub(crate) struct PromptEditor {
    pub(crate) text: String,
    pub(crate) cursor: usize,
}

impl PromptEditor {
    pub(crate) fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub(crate) fn insert_paste(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let pasted: String = normalized
            .chars()
            .filter(|ch| !ch.is_control() || *ch == '\n')
            .collect();
        self.text.insert_str(self.cursor, &pasted);
        self.cursor += pasted.len();
    }

    pub(crate) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
        self.text.drain(previous..self.cursor);
        self.cursor = previous;
    }

    pub(crate) fn delete(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.text.drain(self.cursor..self.cursor + c.len_utf8());
        }
    }

    pub(crate) fn left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
    }

    pub(crate) fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    pub(crate) fn line_home(&mut self) {
        self.cursor -= self.text[..self.cursor]
            .rfind('\n')
            .map_or(self.cursor, |index| self.cursor - index - 1);
    }

    pub(crate) fn line_end(&mut self) {
        self.cursor += self.text[self.cursor..]
            .find('\n')
            .unwrap_or(self.text.len() - self.cursor);
    }

    pub(crate) fn has_file_mention(&self) -> bool {
        self.mention_query().is_some()
    }

    pub(crate) fn mention_query(&self) -> Option<&str> {
        self.text[..self.cursor]
            .rfind('@')
            .filter(|at| !self.text[*at + 1..self.cursor].contains(char::is_whitespace))
            .map(|at| &self.text[at + 1..self.cursor])
    }

    pub(crate) fn complete_file(&mut self, workspace: &Path) {
        let Some(at) = self.text[..self.cursor].rfind('@') else {
            return;
        };
        let query = &self.text[at + 1..self.cursor];
        let Some(candidate) = workspace_files(workspace)
            .into_iter()
            .find(|path| path.starts_with(query))
        else {
            return;
        };
        self.text.replace_range(at + 1..self.cursor, &candidate);
        self.cursor = at + 1 + candidate.len();
    }
}

pub(crate) fn workspace_files(workspace: &Path) -> Vec<String> {
    pub(crate) fn visit(root: &Path, dir: &Path, files: &mut Vec<String>, depth: usize) {
        if depth > 8 || files.len() >= 2_000 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.')
                || matches!(name.to_str(), Some("target" | "node_modules" | "vendor"))
            {
                continue;
            }
            if path.is_dir() {
                visit(root, &path, files, depth + 1);
            } else if path.is_file()
                && let Ok(relative) = path.strip_prefix(root)
            {
                files.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut files = Vec::new();
    visit(workspace, workspace, &mut files, 0);
    files.sort();
    files
}

pub(crate) fn expand_file_mentions(
    message: &str,
    workspace: &Path,
) -> (String, Vec<crate::reducer::FileAttachment>) {
    let attachments = mentioned_file_attachments(message, workspace);
    let Ok(workspace) = workspace.canonicalize() else {
        return (message.to_owned(), attachments);
    };
    let mut expanded = message.to_owned();
    for token in message.split_whitespace() {
        let Some(path) = token.strip_prefix('@') else {
            continue;
        };
        let path = path.trim_end_matches(['.', ',', ':', ';', ')', ']']);
        if path.is_empty() || path.contains(['*', '?']) {
            continue;
        }
        let candidate = workspace.join(path);
        let Ok(canonical) = candidate.canonicalize() else {
            continue;
        };
        if !canonical.starts_with(&workspace) || !canonical.is_file() {
            continue;
        }
        let Ok(metadata) = canonical.metadata() else {
            continue;
        };
        if metadata.len() > 128 * 1024 {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&canonical) else {
            continue;
        };
        let reference = format!("@{path}");
        let attachment = format!(
            "{reference}\n\n<attached file: {path}>\n```\n{contents}\n```\n</attached file>"
        );
        expanded = expanded.replacen(&reference, &attachment, 1);
    }
    (expanded, attachments)
}

pub(crate) fn mentioned_file_attachments(
    message: &str,
    workspace: &Path,
) -> Vec<crate::reducer::FileAttachment> {
    let Ok(workspace) = workspace.canonicalize() else {
        return Vec::new();
    };
    let mut attachments = Vec::new();
    for token in message.split_whitespace() {
        let Some(path) = token.strip_prefix('@') else {
            continue;
        };
        let path = path.trim_end_matches(['.', ',', ':', ';', ')', ']']);
        if path.is_empty() || path.contains(['*', '?']) {
            continue;
        }
        let candidate = workspace.join(path);
        let Ok(canonical) = candidate.canonicalize() else {
            continue;
        };
        if !canonical.starts_with(&workspace) || !canonical.is_file() {
            continue;
        }
        let Ok(metadata) = canonical.metadata() else {
            continue;
        };
        if metadata.len() > 128 * 1024 {
            continue;
        }
        if std::fs::read_to_string(&canonical).is_err() {
            continue;
        }
        attachments.push(crate::reducer::FileAttachment {
            path: path.to_owned(),
            bytes: metadata.len(),
        });
    }
    attachments
}

pub(crate) fn matching_file_suggestions(file_index: &[String], query: &str) -> Vec<String> {
    file_index
        .iter()
        .filter(|path| path.starts_with(query))
        .take(5)
        .cloned()
        .collect()
}
