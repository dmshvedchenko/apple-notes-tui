use notes_core::{Block, Inline, ListItem, RichDocument};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum EditorTarget {
    Block {
        block_index: usize,
    },
    ListItem {
        block_index: usize,
        item_index: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InlineStyle {
    Bold,
    Italic,
    Underline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TargetKind {
    Paragraph,
    Heading1,
    Heading2,
    Heading3,
    Bullet,
    Numbered,
    Quote,
    Code,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EditorError {
    InvalidTarget,
}

impl fmt::Display for EditorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTarget => f.write_str("invalid editor target"),
        }
    }
}

impl std::error::Error for EditorError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EditorDocument {
    pub(crate) document: RichDocument,
}

impl EditorDocument {
    pub(crate) fn new(mut document: RichDocument) -> Self {
        remove_empty_lists(&mut document);
        ensure_editable_document(&mut document);
        Self { document }
    }

    pub(crate) fn empty() -> Self {
        Self::new(RichDocument {
            blocks: vec![Block::Paragraph(vec![])],
        })
    }

    pub(crate) fn from_plaintext(text: &str) -> Self {
        Self::new(RichDocument {
            blocks: text
                .split('\n')
                .map(|line| Block::Paragraph(inlines_from_text(line.to_owned())))
                .collect(),
        })
    }

    pub(crate) fn first_target(&self) -> EditorTarget {
        first_target(&self.document)
    }

    pub(crate) fn validate_target(&self, target: EditorTarget) -> bool {
        validate_target(&self.document, target)
    }

    pub(crate) fn next_target(&self, target: EditorTarget) -> EditorTarget {
        next_target(&self.document, target)
    }

    pub(crate) fn previous_target(&self, target: EditorTarget) -> EditorTarget {
        previous_target(&self.document, target)
    }

    pub(crate) fn visual_index(&self, target: EditorTarget) -> Option<usize> {
        targets(&self.document)
            .iter()
            .position(|candidate| *candidate == target)
    }

    pub(crate) fn target_text(&self, target: EditorTarget) -> Result<String, EditorError> {
        match target {
            EditorTarget::Block { block_index } => match self.document.blocks.get(block_index) {
                Some(Block::CodeBlock(text)) => Ok(text.clone()),
                Some(
                    Block::Paragraph(items)
                    | Block::Heading1(items)
                    | Block::Heading2(items)
                    | Block::Heading3(items)
                    | Block::Quote(items),
                ) => Ok(text_from_inlines(items)),
                _ => Err(EditorError::InvalidTarget),
            },
            EditorTarget::ListItem {
                block_index,
                item_index,
            } => self
                .list(block_index)?
                .get(item_index)
                .map(|item| text_from_inlines(&item.content))
                .ok_or(EditorError::InvalidTarget),
        }
    }

    pub(crate) fn char_len(&self, target: EditorTarget) -> Result<usize, EditorError> {
        Ok(self.target_text(target)?.chars().count())
    }

    pub(crate) fn clamp_cursor(
        &self,
        target: EditorTarget,
        cursor: usize,
    ) -> Result<usize, EditorError> {
        Ok(cursor.min(self.char_len(target)?))
    }

    pub(crate) fn insert_char(
        &mut self,
        target: EditorTarget,
        cursor: usize,
        ch: char,
    ) -> Result<usize, EditorError> {
        let at = self.clamp_cursor(target, cursor)?;
        if let EditorTarget::Block { block_index } = target {
            if let Some(Block::CodeBlock(text)) = self.document.blocks.get_mut(block_index) {
                text.insert(char_to_byte_index(text, at), ch);
                return Ok(at + 1);
            }
        }
        insert_into_inlines(self.target_inlines_mut(target)?, at, ch);
        Ok(at + 1)
    }

    pub(crate) fn backspace(
        &mut self,
        target: EditorTarget,
        cursor: usize,
    ) -> Result<usize, EditorError> {
        let at = self.clamp_cursor(target, cursor)?;
        if at == 0 {
            return Ok(0);
        }
        if let EditorTarget::Block { block_index } = target {
            if let Some(Block::CodeBlock(text)) = self.document.blocks.get_mut(block_index) {
                delete_from_text(text, at - 1);
                return Ok(at - 1);
            }
        }
        delete_from_inlines(self.target_inlines_mut(target)?, at - 1);
        Ok(at - 1)
    }

    pub(crate) fn delete_char(
        &mut self,
        target: EditorTarget,
        cursor: usize,
    ) -> Result<usize, EditorError> {
        let at = self.clamp_cursor(target, cursor)?;
        if at == self.char_len(target)? {
            return Ok(at);
        }
        if let EditorTarget::Block { block_index } = target {
            if let Some(Block::CodeBlock(text)) = self.document.blocks.get_mut(block_index) {
                delete_from_text(text, at);
                return Ok(at);
            }
        }
        delete_from_inlines(self.target_inlines_mut(target)?, at);
        Ok(at)
    }

    pub(crate) fn toggle_style(
        &mut self,
        target: EditorTarget,
        style: InlineStyle,
    ) -> Result<(), EditorError> {
        let old = self.target_inlines(target)?.clone();
        let replacement = match (style, old.as_slice()) {
            (InlineStyle::Bold, [Inline::Bold(children)])
            | (InlineStyle::Italic, [Inline::Italic(children)])
            | (InlineStyle::Underline, [Inline::Underline(children)]) => children.clone(),
            _ => vec![match style {
                InlineStyle::Bold => Inline::Bold(old),
                InlineStyle::Italic => Inline::Italic(old),
                InlineStyle::Underline => Inline::Underline(old),
            }],
        };
        *self.target_inlines_mut(target)? = replacement;
        Ok(())
    }

    pub(crate) fn set_link(
        &mut self,
        target: EditorTarget,
        url: String,
    ) -> Result<(), EditorError> {
        let old = self.target_inlines(target)?.clone();
        *self.target_inlines_mut(target)? = vec![Inline::Link {
            label: old,
            href: url,
        }];
        Ok(())
    }

    pub(crate) fn insert_after(
        &mut self,
        target: EditorTarget,
    ) -> Result<EditorTarget, EditorError> {
        match target {
            EditorTarget::Block { block_index } => {
                if block_index >= self.document.blocks.len() {
                    return Err(EditorError::InvalidTarget);
                }
                self.document
                    .blocks
                    .insert(block_index + 1, Block::Paragraph(vec![]));
                Ok(EditorTarget::Block {
                    block_index: block_index + 1,
                })
            }
            EditorTarget::ListItem {
                block_index,
                item_index,
            } => {
                let items = self.list_mut(block_index)?;
                if item_index >= items.len() {
                    return Err(EditorError::InvalidTarget);
                }
                items.insert(item_index + 1, ListItem { content: vec![] });
                Ok(EditorTarget::ListItem {
                    block_index,
                    item_index: item_index + 1,
                })
            }
        }
    }

    pub(crate) fn convert_target(
        &mut self,
        target: EditorTarget,
        kind: TargetKind,
    ) -> Result<EditorTarget, EditorError> {
        if !self.validate_target(target) {
            return Err(EditorError::InvalidTarget);
        }
        match target {
            EditorTarget::Block { block_index } => {
                let source = self.document.blocks[block_index].clone();
                let content = block_inlines(source)?;
                self.document.blocks[block_index] = block_from_kind(kind, content);
                let next = if kind.is_list() {
                    EditorTarget::ListItem {
                        block_index,
                        item_index: 0,
                    }
                } else {
                    EditorTarget::Block { block_index }
                };
                Ok(self.normalize_adjacent_lists(next))
            }
            EditorTarget::ListItem {
                block_index,
                item_index,
            } => {
                let source_kind = list_kind(&self.document.blocks[block_index])
                    .ok_or(EditorError::InvalidTarget)?;
                if kind.list_kind() == Some(source_kind) {
                    return Ok(target);
                }
                let source = self.document.blocks.remove(block_index);
                let mut source_items = take_list(source).ok_or(EditorError::InvalidTarget)?;
                let after = source_items.split_off(item_index + 1);
                let current = source_items.pop().ok_or(EditorError::InvalidTarget)?;
                let before = source_items;
                let replacement = block_from_kind(kind, current.content);
                let mut blocks = Vec::with_capacity(3);
                if !before.is_empty() {
                    blocks.push(list_block(source_kind, before));
                }
                let replacement_index = block_index + blocks.len();
                blocks.push(replacement);
                if !after.is_empty() {
                    blocks.push(list_block(source_kind, after));
                }
                self.document
                    .blocks
                    .splice(block_index..block_index, blocks);
                let next = if kind.is_list() {
                    EditorTarget::ListItem {
                        block_index: replacement_index,
                        item_index: 0,
                    }
                } else {
                    EditorTarget::Block {
                        block_index: replacement_index,
                    }
                };
                Ok(self.normalize_adjacent_lists(next))
            }
        }
    }

    pub(crate) fn delete_target(
        &mut self,
        target: EditorTarget,
    ) -> Result<EditorTarget, EditorError> {
        let old_targets = targets(&self.document);
        let position = old_targets
            .iter()
            .position(|candidate| *candidate == target)
            .ok_or(EditorError::InvalidTarget)?;
        match target {
            EditorTarget::Block { block_index } => {
                self.document.blocks.remove(block_index);
            }
            EditorTarget::ListItem {
                block_index,
                item_index,
            } => {
                let items = self.list_mut(block_index)?;
                items.remove(item_index);
                if items.is_empty() {
                    self.document.blocks.remove(block_index);
                }
            }
        }
        ensure_editable_document(&mut self.document);
        let first = first_target(&self.document);
        let _ = self.normalize_adjacent_lists(first);
        let remaining = targets(&self.document);
        Ok(remaining[position.min(remaining.len() - 1)])
    }

    fn normalize_adjacent_lists(&mut self, mut target: EditorTarget) -> EditorTarget {
        let mut index = 0;
        while index < self.document.blocks.len() {
            if list_len(&self.document.blocks[index]) == Some(0) {
                self.document.blocks.remove(index);
                target = remap_after_block_removal(target, index);
                continue;
            }
            let can_merge = index + 1 < self.document.blocks.len()
                && list_kind(&self.document.blocks[index]).is_some()
                && list_kind(&self.document.blocks[index])
                    == list_kind(&self.document.blocks[index + 1]);
            if can_merge {
                let left_len = list_len(&self.document.blocks[index]).unwrap_or(0);
                let right = self.document.blocks.remove(index + 1);
                let right_items = take_list(right).unwrap_or_default();
                list_items_mut(&mut self.document.blocks[index])
                    .expect("matching list kind")
                    .extend(right_items);
                target = remap_after_list_merge(target, index, left_len);
                continue;
            }
            index += 1;
        }
        ensure_editable_document(&mut self.document);
        if validate_target(&self.document, target) {
            target
        } else {
            first_target(&self.document)
        }
    }

    fn target_inlines(&self, target: EditorTarget) -> Result<&Vec<Inline>, EditorError> {
        match target {
            EditorTarget::Block { block_index } => match self.document.blocks.get(block_index) {
                Some(
                    Block::Paragraph(items)
                    | Block::Heading1(items)
                    | Block::Heading2(items)
                    | Block::Heading3(items)
                    | Block::Quote(items),
                ) => Ok(items),
                _ => Err(EditorError::InvalidTarget),
            },
            EditorTarget::ListItem {
                block_index,
                item_index,
            } => self
                .list(block_index)?
                .get(item_index)
                .map(|item| &item.content)
                .ok_or(EditorError::InvalidTarget),
        }
    }

    fn target_inlines_mut(
        &mut self,
        target: EditorTarget,
    ) -> Result<&mut Vec<Inline>, EditorError> {
        match target {
            EditorTarget::Block { block_index } => {
                match self.document.blocks.get_mut(block_index) {
                    Some(
                        Block::Paragraph(items)
                        | Block::Heading1(items)
                        | Block::Heading2(items)
                        | Block::Heading3(items)
                        | Block::Quote(items),
                    ) => Ok(items),
                    _ => Err(EditorError::InvalidTarget),
                }
            }
            EditorTarget::ListItem {
                block_index,
                item_index,
            } => self
                .list_mut(block_index)?
                .get_mut(item_index)
                .map(|item| &mut item.content)
                .ok_or(EditorError::InvalidTarget),
        }
    }

    fn list(&self, index: usize) -> Result<&Vec<ListItem>, EditorError> {
        list_items(
            self.document
                .blocks
                .get(index)
                .ok_or(EditorError::InvalidTarget)?,
        )
        .ok_or(EditorError::InvalidTarget)
    }

    fn list_mut(&mut self, index: usize) -> Result<&mut Vec<ListItem>, EditorError> {
        list_items_mut(
            self.document
                .blocks
                .get_mut(index)
                .ok_or(EditorError::InvalidTarget)?,
        )
        .ok_or(EditorError::InvalidTarget)
    }
}

impl TargetKind {
    fn list_kind(self) -> Option<ListKind> {
        match self {
            Self::Bullet => Some(ListKind::Bullet),
            Self::Numbered => Some(ListKind::Numbered),
            _ => None,
        }
    }

    fn is_list(self) -> bool {
        self.list_kind().is_some()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ListKind {
    Bullet,
    Numbered,
}

pub(crate) fn char_to_byte_index(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map(|(offset, _)| offset)
        .unwrap_or(text.len())
}

fn delete_from_text(text: &mut String, at: usize) {
    let start = char_to_byte_index(text, at);
    let end = char_to_byte_index(text, at + 1);
    text.replace_range(start..end, "");
}

fn insert_into_inlines(items: &mut Vec<Inline>, at: usize, ch: char) {
    if items.is_empty() {
        items.push(Inline::Text(ch.to_string()));
        return;
    }
    let mut remaining = at;
    for item in items.iter_mut() {
        let len = inline_char_len(item);
        if remaining <= len {
            insert_into_inline(item, remaining, ch);
            return;
        }
        remaining -= len;
    }
    items.push(Inline::Text(ch.to_string()));
}

fn insert_into_inline(item: &mut Inline, at: usize, ch: char) {
    match item {
        Inline::Text(text) => text.insert(char_to_byte_index(text, at), ch),
        Inline::Bold(children)
        | Inline::Italic(children)
        | Inline::Underline(children)
        | Inline::Strikethrough(children) => insert_into_inlines(children, at, ch),
        Inline::Link { label, .. } => insert_into_inlines(label, at, ch),
    }
}

fn delete_from_inlines(items: &mut [Inline], at: usize) {
    let mut remaining = at;
    for item in items {
        let len = inline_char_len(item);
        if remaining < len {
            delete_from_inline(item, remaining);
            return;
        }
        remaining = remaining.saturating_sub(len);
    }
}

fn delete_from_inline(item: &mut Inline, at: usize) {
    match item {
        Inline::Text(text) => delete_from_text(text, at),
        Inline::Bold(children)
        | Inline::Italic(children)
        | Inline::Underline(children)
        | Inline::Strikethrough(children) => delete_from_inlines(children, at),
        Inline::Link { label, .. } => delete_from_inlines(label, at),
    }
}

fn inline_char_len(item: &Inline) -> usize {
    match item {
        Inline::Text(text) => text.chars().count(),
        Inline::Bold(children)
        | Inline::Italic(children)
        | Inline::Underline(children)
        | Inline::Strikethrough(children) => inlines_char_len(children),
        Inline::Link { label, .. } => inlines_char_len(label),
    }
}

fn inlines_char_len(items: &[Inline]) -> usize {
    items.iter().map(inline_char_len).sum()
}

fn text_from_inlines(items: &[Inline]) -> String {
    let mut text = String::new();
    append_inline_text(items, &mut text);
    text
}

fn append_inline_text(items: &[Inline], text: &mut String) {
    for item in items {
        match item {
            Inline::Text(value) => text.push_str(value),
            Inline::Bold(children)
            | Inline::Italic(children)
            | Inline::Underline(children)
            | Inline::Strikethrough(children) => append_inline_text(children, text),
            Inline::Link { label, .. } => append_inline_text(label, text),
        }
    }
}

fn inlines_from_text(text: String) -> Vec<Inline> {
    if text.is_empty() {
        vec![]
    } else {
        vec![Inline::Text(text)]
    }
}

fn block_inlines(block: Block) -> Result<Vec<Inline>, EditorError> {
    match block {
        Block::Paragraph(items)
        | Block::Heading1(items)
        | Block::Heading2(items)
        | Block::Heading3(items)
        | Block::Quote(items) => Ok(items),
        Block::CodeBlock(text) => Ok(inlines_from_text(text)),
        Block::BulletList(_) | Block::NumberedList(_) => Err(EditorError::InvalidTarget),
    }
}

fn block_from_kind(kind: TargetKind, content: Vec<Inline>) -> Block {
    match kind {
        TargetKind::Paragraph => Block::Paragraph(content),
        TargetKind::Heading1 => Block::Heading1(content),
        TargetKind::Heading2 => Block::Heading2(content),
        TargetKind::Heading3 => Block::Heading3(content),
        TargetKind::Bullet => Block::BulletList(vec![ListItem { content }]),
        TargetKind::Numbered => Block::NumberedList(vec![ListItem { content }]),
        TargetKind::Quote => Block::Quote(content),
        TargetKind::Code => Block::CodeBlock(text_from_inlines(&content)),
    }
}

fn list_kind(block: &Block) -> Option<ListKind> {
    match block {
        Block::BulletList(_) => Some(ListKind::Bullet),
        Block::NumberedList(_) => Some(ListKind::Numbered),
        _ => None,
    }
}

fn list_len(block: &Block) -> Option<usize> {
    list_items(block).map(Vec::len)
}

fn list_items(block: &Block) -> Option<&Vec<ListItem>> {
    match block {
        Block::BulletList(items) | Block::NumberedList(items) => Some(items),
        _ => None,
    }
}

fn list_items_mut(block: &mut Block) -> Option<&mut Vec<ListItem>> {
    match block {
        Block::BulletList(items) | Block::NumberedList(items) => Some(items),
        _ => None,
    }
}

fn take_list(block: Block) -> Option<Vec<ListItem>> {
    match block {
        Block::BulletList(items) | Block::NumberedList(items) => Some(items),
        _ => None,
    }
}

fn list_block(kind: ListKind, items: Vec<ListItem>) -> Block {
    match kind {
        ListKind::Bullet => Block::BulletList(items),
        ListKind::Numbered => Block::NumberedList(items),
    }
}

fn remove_empty_lists(document: &mut RichDocument) {
    document
        .blocks
        .retain(|block| !matches!(list_len(block), Some(0)));
}

fn ensure_editable_document(document: &mut RichDocument) {
    if document.blocks.is_empty() {
        document.blocks.push(Block::Paragraph(vec![]));
    }
}

fn remap_after_block_removal(target: EditorTarget, removed: usize) -> EditorTarget {
    match target {
        EditorTarget::Block { block_index } if block_index > removed => EditorTarget::Block {
            block_index: block_index - 1,
        },
        EditorTarget::ListItem {
            block_index,
            item_index,
        } if block_index > removed => EditorTarget::ListItem {
            block_index: block_index - 1,
            item_index,
        },
        other => other,
    }
}

fn remap_after_list_merge(
    target: EditorTarget,
    left_block: usize,
    left_len: usize,
) -> EditorTarget {
    match target {
        EditorTarget::ListItem {
            block_index,
            item_index,
        } if block_index == left_block + 1 => EditorTarget::ListItem {
            block_index: left_block,
            item_index: left_len + item_index,
        },
        EditorTarget::Block { block_index } if block_index > left_block + 1 => {
            EditorTarget::Block {
                block_index: block_index - 1,
            }
        }
        EditorTarget::ListItem {
            block_index,
            item_index,
        } if block_index > left_block + 1 => EditorTarget::ListItem {
            block_index: block_index - 1,
            item_index,
        },
        other => other,
    }
}

pub(crate) fn validate_target(document: &RichDocument, target: EditorTarget) -> bool {
    match target {
        EditorTarget::Block { block_index } => matches!(
            document.blocks.get(block_index),
            Some(
                Block::Paragraph(_)
                    | Block::Heading1(_)
                    | Block::Heading2(_)
                    | Block::Heading3(_)
                    | Block::Quote(_)
                    | Block::CodeBlock(_)
            )
        ),
        EditorTarget::ListItem {
            block_index,
            item_index,
        } => matches!(
            document.blocks.get(block_index),
            Some(Block::BulletList(items) | Block::NumberedList(items)) if item_index < items.len()
        ),
    }
}

pub(crate) fn first_target(document: &RichDocument) -> EditorTarget {
    targets(document)
        .first()
        .copied()
        .unwrap_or(EditorTarget::Block { block_index: 0 })
}

pub(crate) fn next_target(document: &RichDocument, target: EditorTarget) -> EditorTarget {
    let all = targets(document);
    let position = all
        .iter()
        .position(|candidate| *candidate == target)
        .unwrap_or(0);
    all.get((position + 1).min(all.len().saturating_sub(1)))
        .copied()
        .unwrap_or(EditorTarget::Block { block_index: 0 })
}

pub(crate) fn previous_target(document: &RichDocument, target: EditorTarget) -> EditorTarget {
    let all = targets(document);
    let position = all
        .iter()
        .position(|candidate| *candidate == target)
        .unwrap_or(0);
    all.get(position.saturating_sub(1))
        .copied()
        .unwrap_or(EditorTarget::Block { block_index: 0 })
}

fn targets(document: &RichDocument) -> Vec<EditorTarget> {
    document
        .blocks
        .iter()
        .enumerate()
        .flat_map(|(block_index, block)| match block {
            Block::BulletList(items) | Block::NumberedList(items) => items
                .iter()
                .enumerate()
                .map(move |(item_index, _)| EditorTarget::ListItem {
                    block_index,
                    item_index,
                })
                .collect(),
            _ => vec![EditorTarget::Block { block_index }],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Vec<Inline> {
        inlines_from_text(value.to_owned())
    }

    fn item(value: &str) -> ListItem {
        ListItem {
            content: text(value),
        }
    }

    fn target(block_index: usize) -> EditorTarget {
        EditorTarget::Block { block_index }
    }

    fn list_target(block_index: usize, item_index: usize) -> EditorTarget {
        EditorTarget::ListItem {
            block_index,
            item_index,
        }
    }

    #[test]
    fn unicode_typing_backspace_and_delete_are_char_safe() {
        let mut editor = EditorDocument::from_plaintext("Grüße ✅ русский");
        let target = editor.first_target();
        let end = editor.char_len(target).unwrap();
        assert_eq!(editor.insert_char(target, end, '🍎').unwrap(), end + 1);
        assert_eq!(editor.backspace(target, end + 1).unwrap(), end);
        let check = "Grüße ".chars().count();
        editor.delete_char(target, check).unwrap();
        assert_eq!(editor.target_text(target).unwrap(), "Grüße  русский");
    }

    #[test]
    fn mixed_inline_edit_preserves_neighbor_styles() {
        let mut editor = EditorDocument::new(RichDocument {
            blocks: vec![Block::Paragraph(vec![
                Inline::Bold(text("Bold")),
                Inline::Text(" ".into()),
                Inline::Italic(text("italic")),
            ])],
        });
        editor.insert_char(target(0), 2, 'X').unwrap();
        assert_eq!(editor.target_text(target(0)).unwrap(), "BoXld italic");
        assert!(matches!(
            &editor.document.blocks[0],
            Block::Paragraph(items)
                if matches!(&items[0], Inline::Bold(children) if text_from_inlines(children) == "BoXld")
                    && matches!(&items[2], Inline::Italic(children) if text_from_inlines(children) == "italic")
        ));
    }

    #[test]
    fn target_navigation_visits_individual_list_items() {
        let editor = EditorDocument::new(RichDocument {
            blocks: vec![
                Block::Paragraph(text("before")),
                Block::BulletList(vec![item("one"), item("two")]),
                Block::CodeBlock("after".into()),
            ],
        });
        assert_eq!(editor.next_target(target(0)), list_target(1, 0));
        assert_eq!(editor.next_target(list_target(1, 0)), list_target(1, 1));
        assert_eq!(editor.next_target(list_target(1, 1)), target(2));
        assert_eq!(editor.previous_target(target(2)), list_target(1, 1));
    }

    #[test]
    fn list_item_mutations_are_isolated() {
        let mut editor = EditorDocument::new(RichDocument {
            blocks: vec![Block::BulletList(vec![item("Alpha"), item("Beta")])],
        });
        let second = list_target(0, 1);
        editor.insert_char(second, 4, 'X').unwrap();
        editor.backspace(second, 5).unwrap();
        editor.delete_char(second, 0).unwrap();
        editor.toggle_style(second, InlineStyle::Bold).unwrap();
        assert_eq!(editor.target_text(list_target(0, 0)).unwrap(), "Alpha");
        assert_eq!(editor.target_text(second).unwrap(), "eta");
        assert!(matches!(
            &editor.document.blocks[0],
            Block::BulletList(items) if matches!(&items[1].content[..], [Inline::Bold(_)])
        ));
    }

    #[test]
    fn enter_inserts_an_item_inside_each_list_kind() {
        for block in [
            Block::BulletList(vec![item("one"), item("two")]),
            Block::NumberedList(vec![item("one"), item("two")]),
        ] {
            let mut editor = EditorDocument::new(RichDocument {
                blocks: vec![block],
            });
            let inserted = editor.insert_after(list_target(0, 0)).unwrap();
            assert_eq!(inserted, list_target(0, 1));
            assert_eq!(editor.target_text(inserted).unwrap(), "");
            assert_eq!(editor.target_text(list_target(0, 2)).unwrap(), "two");
        }
    }

    #[test]
    fn middle_list_item_splits_into_an_ordinary_block() {
        for kind in [
            TargetKind::Paragraph,
            TargetKind::Heading1,
            TargetKind::Heading2,
            TargetKind::Heading3,
            TargetKind::Quote,
            TargetKind::Code,
        ] {
            let mut editor = EditorDocument::new(RichDocument {
                blocks: vec![Block::BulletList(vec![
                    item("one"),
                    item("TWO"),
                    item("three"),
                ])],
            });
            let converted = editor.convert_target(list_target(0, 1), kind).unwrap();
            assert_eq!(converted, target(1));
            assert_eq!(editor.target_text(converted).unwrap(), "TWO");
            assert_eq!(editor.target_text(list_target(0, 0)).unwrap(), "one");
            assert_eq!(editor.target_text(list_target(2, 0)).unwrap(), "three");
        }
    }

    #[test]
    fn list_split_handles_first_last_only_and_numbered_items() {
        for (source, item_index, expected_target, expected_blocks) in [
            (
                Block::BulletList(vec![item("one"), item("two"), item("three")]),
                0,
                target(0),
                2,
            ),
            (
                Block::BulletList(vec![item("one"), item("two"), item("three")]),
                2,
                target(1),
                2,
            ),
            (Block::BulletList(vec![item("only")]), 0, target(0), 1),
            (
                Block::NumberedList(vec![item("one"), item("two"), item("three")]),
                1,
                target(1),
                3,
            ),
            (Block::NumberedList(vec![item("only")]), 0, target(0), 1),
        ] {
            let expected_text = match &source {
                Block::BulletList(items) | Block::NumberedList(items) => {
                    text_from_inlines(&items[item_index].content)
                }
                _ => unreachable!(),
            };
            let mut editor = EditorDocument::new(RichDocument {
                blocks: vec![source],
            });
            let converted = editor
                .convert_target(list_target(0, item_index), TargetKind::Paragraph)
                .unwrap();
            assert_eq!(converted, expected_target);
            assert_eq!(editor.document.blocks.len(), expected_blocks);
            assert_eq!(editor.target_text(converted).unwrap(), expected_text);
            assert!(editor.validate_target(converted));
        }
    }

    #[test]
    fn list_kind_conversion_moves_only_the_current_item() {
        let mut editor = EditorDocument::new(RichDocument {
            blocks: vec![Block::BulletList(vec![
                item("one"),
                item("TWO"),
                item("three"),
            ])],
        });
        let converted = editor
            .convert_target(list_target(0, 1), TargetKind::Numbered)
            .unwrap();
        assert_eq!(converted, list_target(1, 0));
        assert!(matches!(editor.document.blocks[0], Block::BulletList(_)));
        assert!(matches!(editor.document.blocks[1], Block::NumberedList(_)));
        assert!(matches!(editor.document.blocks[2], Block::BulletList(_)));
        assert_eq!(editor.target_text(converted).unwrap(), "TWO");
        let converted = editor
            .convert_target(converted, TargetKind::Bullet)
            .unwrap();
        assert_eq!(converted, list_target(0, 1));
        assert_eq!(editor.document.blocks.len(), 1);
    }

    #[test]
    fn paragraph_to_list_merges_neighbors_and_remaps_target() {
        for (kind, block) in [
            (TargetKind::Bullet, Block::BulletList(vec![item("one")])),
            (TargetKind::Numbered, Block::NumberedList(vec![item("one")])),
        ] {
            let trailing = match block {
                Block::BulletList(_) => Block::BulletList(vec![item("three")]),
                Block::NumberedList(_) => Block::NumberedList(vec![item("three")]),
                _ => unreachable!(),
            };
            let mut editor = EditorDocument::new(RichDocument {
                blocks: vec![block, Block::Paragraph(text("two")), trailing],
            });
            let converted = editor.convert_target(target(1), kind).unwrap();
            assert_eq!(converted, list_target(0, 1));
            assert_eq!(editor.document.blocks.len(), 1);
            assert_eq!(editor.target_text(list_target(0, 0)).unwrap(), "one");
            assert_eq!(editor.target_text(list_target(0, 1)).unwrap(), "two");
            assert_eq!(editor.target_text(list_target(0, 2)).unwrap(), "three");
        }
    }

    #[test]
    fn delete_target_prefers_next_then_previous_and_keeps_document_editable() {
        let mut editor = EditorDocument::new(RichDocument {
            blocks: vec![
                Block::Paragraph(text("before")),
                Block::BulletList(vec![item("one"), item("two")]),
                Block::Paragraph(text("after")),
            ],
        });
        let next = editor.delete_target(list_target(1, 0)).unwrap();
        assert_eq!(next, list_target(1, 0));
        assert_eq!(editor.target_text(next).unwrap(), "two");
        let next = editor.delete_target(next).unwrap();
        assert_eq!(next, target(1));
        assert_eq!(editor.target_text(next).unwrap(), "after");
        editor.delete_target(next).unwrap();
        let only = editor.delete_target(target(0)).unwrap();
        assert_eq!(only, target(0));
        assert_eq!(editor.target_text(only).unwrap(), "");
    }

    #[test]
    fn code_blocks_are_editable_and_convertible() {
        let mut editor = EditorDocument::new(RichDocument {
            blocks: vec![Block::CodeBlock("日本語".into())],
        });
        let target = target(0);
        editor.insert_char(target, 3, '✅').unwrap();
        assert_eq!(editor.target_text(target).unwrap(), "日本語✅");
        let target = editor
            .convert_target(target, TargetKind::Paragraph)
            .unwrap();
        assert!(matches!(editor.document.blocks[0], Block::Paragraph(_)));
        assert_eq!(editor.target_text(target).unwrap(), "日本語✅");
    }

    #[test]
    fn preservation_sequence_keeps_semantic_order_and_formatting() {
        let mut editor = EditorDocument::new(RichDocument {
            blocks: vec![
                Block::Heading1(text("Title")),
                Block::Paragraph(text("Intro")),
                Block::BulletList(vec![item("one"), item("two"), item("three")]),
                Block::Paragraph(text("Middle")),
                Block::NumberedList(vec![item("first"), item("second")]),
                Block::Quote(text("End")),
                Block::CodeBlock("Tail".into()),
            ],
        });
        editor
            .toggle_style(list_target(2, 1), InlineStyle::Bold)
            .unwrap();
        let extracted = editor
            .convert_target(list_target(2, 1), TargetKind::Paragraph)
            .unwrap();
        let numbered = editor
            .convert_target(extracted, TargetKind::Numbered)
            .unwrap();
        let bullet = editor
            .convert_target(list_target(6, 0), TargetKind::Bullet)
            .unwrap();

        assert_eq!(numbered, list_target(3, 0));
        assert_eq!(bullet, list_target(6, 0));
        assert_eq!(editor.document.blocks.len(), 10);
        let texts: Vec<_> = targets(&editor.document)
            .into_iter()
            .map(|target| editor.target_text(target).unwrap())
            .collect();
        assert_eq!(
            texts,
            [
                "Title", "Intro", "one", "two", "three", "Middle", "first", "second", "End",
                "Tail",
            ]
        );
        assert!(matches!(
            &editor.document.blocks[3],
            Block::NumberedList(items)
                if matches!(&items[0].content[..], [Inline::Bold(_)])
        ));
        assert!(matches!(editor.document.blocks[6], Block::BulletList(_)));
        assert!(matches!(editor.document.blocks[7], Block::NumberedList(_)));
        assert!(matches!(editor.document.blocks[8], Block::Quote(_)));
        assert!(matches!(editor.document.blocks[9], Block::CodeBlock(_)));
    }
}
