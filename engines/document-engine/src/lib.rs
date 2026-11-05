//! UI-independent document model and crash-recoverable persistence session.
//!
//! The first persistence milestone journals a complete document snapshot for
//! each edit. This is intentionally simple and safe; compact edit operations
//! can replace snapshots after a stable document format is established.

/// Return the engine's stable diagnostic name for application startup logs.
pub const fn engine_name() -> &'static str {
    "document-engine"
}

use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use suite_common::autosave::AutosaveScheduler;
use suite_common::checkpoint::{Checkpoint, read_checkpoint, verify_checkpoint, write_checkpoint};
use suite_common::recovery::RecoveryJournal;
use suite_common::storage::{FileSystem, LocalFileSystem};

const DOCUMENT_MAGIC: &[u8; 8] = b"LOUSDOC1";
const DOCUMENT_FORMAT_VERSION: u32 = 7;
const PREVIOUS_DOCUMENT_FORMAT_VERSION: u32 = 6;
const BLOCK_ORDER_FORMAT_VERSION: u32 = 6;
const TABLE_FORMATTING_FORMAT_VERSION: u32 = 7;
const INLINE_MARK_FORMAT_VERSION: u32 = 4;
const LEGACY_TABLE_FORMAT_VERSION: u32 = 5;
const LEGACY_LIST_FORMAT_VERSION: u32 = 3;
const LEGACY_STRUCTURED_FORMAT_VERSION: u32 = 2;
const LEGACY_DOCUMENT_FORMAT_VERSION: u32 = 1;
const DOCUMENT_HEADER_SIZE: usize = 32;
const MAX_DOCUMENT_SIZE: usize = 64 * 1024 * 1024 - DOCUMENT_HEADER_SIZE;
const MAX_TITLE_SIZE: usize = 1024 * 1024;
const CHECKPOINT_FILE: &str = "document.checkpoint";
const JOURNAL_FILE: &str = "document.journal";

/// Semantic formatting applied to a paragraph block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ParagraphStyle {
    #[default]
    Normal = 0,
    Heading1 = 1,
    Heading2 = 2,
    Quote = 3,
    Code = 4,
}

impl ParagraphStyle {
    fn from_byte(value: u8) -> io::Result<Self> {
        match value {
            0 => Ok(Self::Normal),
            1 => Ok(Self::Heading1),
            2 => Ok(Self::Heading2),
            3 => Ok(Self::Quote),
            4 => Ok(Self::Code),
            _ => Err(invalid_data("unknown paragraph style")),
        }
    }
}

/// List semantics attached to a paragraph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ListKind {
    #[default]
    None = 0,
    Bullet = 1,
    Numbered = 2,
}

impl ListKind {
    fn from_byte(value: u8) -> io::Result<Self> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Bullet),
            2 => Ok(Self::Numbered),
            _ => Err(invalid_data("unknown paragraph list kind")),
        }
    }
}

/// Character-level formatting marks stored as independent UTF-8 byte ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum InlineMark {
    Bold = 1,
    Italic = 2,
    Underline = 3,
}

impl InlineMark {
    fn from_byte(value: u8) -> io::Result<Self> {
        match value {
            1 => Ok(Self::Bold),
            2 => Ok(Self::Italic),
            3 => Ok(Self::Underline),
            _ => Err(invalid_data("unknown inline formatting mark")),
        }
    }
}

/// A half-open UTF-8 byte range with one character-level formatting mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSpan {
    start: usize,
    end: usize,
    mark: InlineMark,
}

impl TextSpan {
    pub fn start(&self) -> usize {
        self.start
    }
    pub fn end(&self) -> usize {
        self.end
    }
    pub fn mark(&self) -> InlineMark {
        self.mark
    }
}

/// A stable, independently addressable paragraph block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paragraph {
    id: u64,
    text: String,
    style: ParagraphStyle,
    list_kind: ListKind,
    list_level: u8,
    spans: Vec<TextSpan>,
}

impl Paragraph {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn style(&self) -> ParagraphStyle {
        self.style
    }
    pub fn list_kind(&self) -> ListKind {
        self.list_kind
    }
    pub fn list_level(&self) -> u8 {
        self.list_level
    }
    pub fn spans(&self) -> &[TextSpan] {
        &self.spans
    }
}

/// Horizontal alignment used by a table cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum CellAlignment {
    #[default]
    Left = 0,
    Center = 1,
    Right = 2,
}

impl CellAlignment {
    fn from_byte(value: u8) -> io::Result<Self> {
        match value {
            0 => Ok(Self::Left),
            1 => Ok(Self::Center),
            2 => Ok(Self::Right),
            _ => Err(invalid_data("unknown table cell alignment")),
        }
    }
}

/// A table cell with stable identity, text and persistent presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableCell {
    id: u64,
    text: String,
    alignment: CellAlignment,
    background: Option<u32>,
    borders: u8,
}
impl TableCell {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn alignment(&self) -> CellAlignment {
        self.alignment
    }
    /// Optional 24-bit RGB background color (0xRRGGBB).
    pub fn background(&self) -> Option<u32> {
        self.background
    }
    /// Border bit mask: top=1, right=2, bottom=4, left=8.
    pub fn borders(&self) -> u8 {
        self.borders
    }
}
/// A row in a rectangular table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    id: u64,
    cells: Vec<TableCell>,
    height_twips: u32,
}
impl TableRow {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn cells(&self) -> &[TableCell] {
        &self.cells
    }
    /// Zero means automatic row height; otherwise the value is in twips.
    pub fn height_twips(&self) -> u32 {
        self.height_twips
    }
}
/// A rectangular table with stable table, row and cell IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    id: u64,
    rows: Vec<TableRow>,
    columns: usize,
    column_widths_twips: Vec<u32>,
}
impl Table {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn rows(&self) -> &[TableRow] {
        &self.rows
    }
    pub fn columns(&self) -> usize {
        self.columns
    }
    /// Column widths in twips (1/20 point).
    pub fn column_widths_twips(&self) -> &[u32] {
        &self.column_widths_twips
    }
}
/// A reference to a block in the document's persistent reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocumentBlock {
    Paragraph(u64),
    Table(u64),
}

/// Editable document with stable IDs and a compatibility plain-text view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    title: String,
    body: String,
    revision: u64,
    paragraphs: Vec<Paragraph>,
    tables: Vec<Table>,
    block_order: Vec<DocumentBlock>,
    next_block_id: u64,
}

impl Document {
    /// Build an editable document from plain text, preserving every line as a paragraph.
    pub fn from_plain_text(title: impl Into<String>, text: &str) -> io::Result<Self> {
        if text.len() > MAX_DOCUMENT_SIZE {
            return Err(invalid_input("plain text exceeds the document size limit"));
        }
        let mut document = Self::new(title)?;
        document.paragraphs.clear();
        document.block_order.clear();
        for (index, line) in text.split('\n').enumerate() {
            let id = u64::try_from(index)
                .ok()
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| invalid_input("paragraph ID space exhausted"))?;
            let line = line.strip_suffix('\r').unwrap_or(line);
            document.paragraphs.push(Paragraph {
                id,
                text: line.to_owned(),
                style: ParagraphStyle::Normal,
                list_kind: ListKind::None,
                list_level: 0,
                spans: Vec::new(),
            });
            document.block_order.push(DocumentBlock::Paragraph(id));
        }
        document.body = document
            .paragraphs
            .iter()
            .map(|paragraph| paragraph.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        document.next_block_id = u64::try_from(document.paragraphs.len())
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| invalid_input("paragraph ID space exhausted"))?;
        validate_document_size(&document)?;
        Ok(document)
    }

    pub fn new(title: impl Into<String>) -> io::Result<Self> {
        let title = title.into();
        if title.len() > MAX_TITLE_SIZE {
            return Err(invalid_input("document title exceeds 1 MiB"));
        }
        Ok(Self {
            title,
            body: String::new(),
            revision: 0,
            paragraphs: vec![Paragraph {
                id: 1,
                text: String::new(),
                style: ParagraphStyle::Normal,
                list_kind: ListKind::None,
                list_level: 0,
                spans: Vec::new(),
            }],
            tables: Vec::new(),
            block_order: vec![DocumentBlock::Paragraph(1)],
            next_block_id: 2,
        })
    }

    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn body(&self) -> &str {
        &self.body
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn paragraphs(&self) -> &[Paragraph] {
        &self.paragraphs
    }
    pub fn tables(&self) -> &[Table] {
        &self.tables
    }
    /// Return paragraph and table references in persistent reading order.
    pub fn blocks(&self) -> &[DocumentBlock] {
        &self.block_order
    }

    fn increment_revision(&self, next: &mut Self) -> io::Result<()> {
        next.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid_input("document revision exhausted"))?;
        Ok(())
    }

    fn with_title(&self, title: String) -> io::Result<Self> {
        if title.len() > MAX_TITLE_SIZE {
            return Err(invalid_input("document title exceeds 1 MiB"));
        }
        let mut next = self.clone();
        self.increment_revision(&mut next)?;
        next.title = title;
        validate_document_size(&next)?;
        Ok(next)
    }

    fn with_body(&self, body: String) -> io::Result<Self> {
        let mut next = self.clone();
        self.increment_revision(&mut next)?;
        let mut paragraphs = Vec::new();
        let mut next_id = self.next_block_id;
        for (index, text) in body.split('\n').enumerate() {
            if let Some(previous) = self.paragraphs.get(index) {
                paragraphs.push(Paragraph {
                    id: previous.id,
                    text: text.to_owned(),
                    style: previous.style,
                    list_kind: previous.list_kind,
                    list_level: previous.list_level,
                    spans: if previous.text == text {
                        previous.spans.clone()
                    } else {
                        Vec::new()
                    },
                });
            } else {
                let id = next_id;
                next_id = next_id
                    .checked_add(1)
                    .ok_or_else(|| invalid_input("paragraph ID space exhausted"))?;
                paragraphs.push(Paragraph {
                    id,
                    text: text.to_owned(),
                    style: ParagraphStyle::Normal,
                    list_kind: ListKind::None,
                    list_level: 0,
                    spans: Vec::new(),
                });
            }
        }
        let paragraph_ids: std::collections::HashSet<u64> =
            paragraphs.iter().map(|p| p.id).collect();
        next.block_order.retain(|block| match block {
            DocumentBlock::Paragraph(id) => paragraph_ids.contains(id),
            DocumentBlock::Table(_) => true,
        });
        for paragraph in &paragraphs {
            let block = DocumentBlock::Paragraph(paragraph.id);
            if !next.block_order.contains(&block) {
                next.block_order.push(block);
            }
        }
        next.body = body;
        next.paragraphs = paragraphs;
        next.next_block_id = next_id;
        validate_document_size(&next)?;
        Ok(next)
    }

    fn with_paragraphs(&self, paragraphs: Vec<Paragraph>, next_block_id: u64) -> io::Result<Self> {
        if paragraphs.is_empty() {
            return Err(invalid_input(
                "a document must contain at least one paragraph",
            ));
        }
        let mut next = self.clone();
        self.increment_revision(&mut next)?;
        let paragraph_ids: std::collections::HashSet<u64> =
            paragraphs.iter().map(|p| p.id).collect();
        next.block_order.retain(|block| match block {
            DocumentBlock::Paragraph(id) => paragraph_ids.contains(id),
            DocumentBlock::Table(_) => true,
        });
        for paragraph in &paragraphs {
            let block = DocumentBlock::Paragraph(paragraph.id);
            if !next.block_order.contains(&block) {
                next.block_order.push(block);
            }
        }
        next.body = paragraphs
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        next.paragraphs = paragraphs;
        next.next_block_id = next_block_id;
        validate_document_size(&next)?;
        Ok(next)
    }
}

/// Encode a document using the current versioned binary format.
/// Count Unicode whitespace-delimited words in paragraphs and table cells.
pub fn word_count(document: &Document) -> io::Result<usize> {
    validate_document_size(document)?;
    let mut count = 0usize;
    for paragraph in &document.paragraphs {
        count = count
            .checked_add(paragraph.text.split_whitespace().count())
            .ok_or_else(|| invalid_data("document word count overflow"))?;
    }
    for table in &document.tables {
        for row in &table.rows {
            for cell in &row.cells {
                count = count
                    .checked_add(cell.text.split_whitespace().count())
                    .ok_or_else(|| invalid_data("document word count overflow"))?;
            }
        }
    }
    Ok(count)
}

/// Count Unicode scalar values in paragraph and table-cell text.
pub fn character_count(document: &Document) -> io::Result<usize> {
    validate_document_size(document)?;
    let mut count = 0usize;
    for paragraph in &document.paragraphs {
        count = count
            .checked_add(paragraph.text.chars().count())
            .ok_or_else(|| invalid_data("document character count overflow"))?;
    }
    for table in &document.tables {
        for row in &table.rows {
            for cell in &row.cells {
                count = count
                    .checked_add(cell.text.chars().count())
                    .ok_or_else(|| invalid_data("document character count overflow"))?;
            }
        }
    }
    Ok(count)
}

/// A literal text match in a paragraph or table cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocumentSearchMatch {
    /// Paragraph ID, or table ID when cell_id is present.
    pub block_id: u64,
    /// Stable table-cell ID; None for paragraph matches.
    pub cell_id: Option<u64>,
    /// UTF-8 byte range in the matched text.
    pub start: usize,
    pub end: usize,
}

/// Find literal, case-sensitive text in paragraphs and table cells.
pub fn find_text(document: &Document, query: &str) -> io::Result<Vec<DocumentSearchMatch>> {
    validate_document_size(document)?;
    if query.is_empty() {
        return Err(invalid_input("text search query cannot be empty"));
    }
    let mut matches = Vec::new();
    for paragraph in &document.paragraphs {
        for (start, _) in paragraph.text.match_indices(query) {
            matches.push(DocumentSearchMatch {
                block_id: paragraph.id,
                cell_id: None,
                start,
                end: start + query.len(),
            });
        }
    }
    for table in &document.tables {
        for row in &table.rows {
            for cell in &row.cells {
                for (start, _) in cell.text.match_indices(query) {
                    matches.push(DocumentSearchMatch {
                        block_id: table.id,
                        cell_id: Some(cell.id),
                        start,
                        end: start + query.len(),
                    });
                }
            }
        }
    }
    Ok(matches)
}

/// Export readable plain text while preserving paragraph order and table rows.
/// Table cells are separated by tabs; rows are separated by newlines.
pub fn export_text(document: &Document) -> io::Result<String> {
    validate_document_size(document)?;
    let mut output = String::new();
    for (block_index, block) in document.block_order.iter().enumerate() {
        if block_index > 0 && !output.ends_with('\n') {
            output.push('\n');
        }
        match *block {
            DocumentBlock::Paragraph(id) => {
                let paragraph = document
                    .paragraphs
                    .iter()
                    .find(|paragraph| paragraph.id == id)
                    .ok_or_else(|| invalid_data("document block references a missing paragraph"))?;
                output.push_str(&paragraph.text);
                output.push('\n');
            }
            DocumentBlock::Table(id) => {
                let table = document
                    .tables
                    .iter()
                    .find(|table| table.id == id)
                    .ok_or_else(|| invalid_data("document block references a missing table"))?;
                for (row_index, row) in table.rows.iter().enumerate() {
                    if row_index > 0 {
                        output.push('\n');
                    }
                    for (column_index, cell) in row.cells.iter().enumerate() {
                        if column_index > 0 {
                            output.push('\t');
                        }
                        output.push_str(&cell.text);
                    }
                }
                output.push('\n');
            }
        }
    }
    while output.ends_with("\n\n") {
        output.pop();
    }
    Ok(output)
}

/// Export the document's editable block order to Markdown.
///
/// Paragraph styles, list semantics, inline emphasis and tables are preserved
/// where Markdown has a direct representation. Underline uses HTML markup.
pub fn export_markdown(document: &Document) -> io::Result<String> {
    validate_document_size(document)?;
    let mut output = String::new();
    if !document.title.is_empty() {
        output.push_str("# ");
        output.push_str(&markdown_escape(&document.title));
        output.push_str("\n\n");
    }

    for (block_index, block) in document.block_order.iter().enumerate() {
        match *block {
            DocumentBlock::Paragraph(id) => {
                let paragraph = document
                    .paragraphs
                    .iter()
                    .find(|paragraph| paragraph.id == id)
                    .ok_or_else(|| invalid_data("document block references a missing paragraph"))?;
                let text = markdown_paragraph_text(paragraph);
                match paragraph.style {
                    ParagraphStyle::Normal => {
                        if paragraph.list_kind == ListKind::Bullet {
                            output.push_str(&"  ".repeat(paragraph.list_level as usize));
                            output.push_str("- ");
                            output.push_str(&text);
                        } else if paragraph.list_kind == ListKind::Numbered {
                            output.push_str(&"  ".repeat(paragraph.list_level as usize));
                            output.push_str("1. ");
                            output.push_str(&text);
                        } else {
                            output.push_str(&text);
                        }
                    }
                    ParagraphStyle::Heading1 => {
                        output.push_str("# ");
                        output.push_str(&text);
                    }
                    ParagraphStyle::Heading2 => {
                        output.push_str("## ");
                        output.push_str(&text);
                    }
                    ParagraphStyle::Quote => {
                        output.push_str("> ");
                        output.push_str(&text);
                    }
                    ParagraphStyle::Code => {
                        output.push_str("    ");
                        output.push_str(&paragraph.text.replace('\n', "\n    "));
                    }
                }
                output.push('\n');
            }
            DocumentBlock::Table(id) => {
                let table = document
                    .tables
                    .iter()
                    .find(|table| table.id == id)
                    .ok_or_else(|| invalid_data("document block references a missing table"))?;
                if table.columns == 0 {
                    continue;
                }
                for (row_index, row) in table.rows.iter().enumerate() {
                    output.push('|');
                    for cell in &row.cells {
                        output.push(' ');
                        output.push_str(&markdown_table_escape(&cell.text));
                        output.push_str(" |");
                    }
                    output.push('\n');
                    if row_index == 0 {
                        output.push('|');
                        for _ in 0..table.columns {
                            output.push_str(" --- |");
                        }
                        output.push('\n');
                    }
                }
            }
        }
        if block_index + 1 < document.block_order.len() {
            output.push('\n');
        }
    }
    Ok(output)
}

/// Export a document as standalone semantic HTML (without external assets).
/// Paragraph formatting and table cell text are retained; unsupported page
/// layout features are intentionally not synthesized.
/// Export paragraphs and tables as UTF-8 plain text.
pub fn export_plain_text(document: &Document) -> io::Result<String> {
    validate_document_size(document)?;
    let mut output = String::new();
    for (index, block) in document.block_order.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        match *block {
            DocumentBlock::Paragraph(id) => {
                let paragraph = document
                    .paragraphs
                    .iter()
                    .find(|paragraph| paragraph.id == id)
                    .ok_or_else(|| invalid_data("document block references a missing paragraph"))?;
                output.push_str(&paragraph.text);
            }
            DocumentBlock::Table(id) => {
                let table = document
                    .tables
                    .iter()
                    .find(|table| table.id == id)
                    .ok_or_else(|| invalid_data("document block references a missing table"))?;
                for (row_index, row) in table.rows.iter().enumerate() {
                    if row_index > 0 {
                        output.push('\n');
                    }
                    for (column_index, cell) in row.cells.iter().enumerate() {
                        if column_index > 0 {
                            output.push('\t');
                        }
                        output.push_str(&cell.text);
                    }
                }
            }
        }
    }
    Ok(output)
}

pub fn export_html(document: &Document) -> io::Result<String> {
    validate_document_size(document)?;
    let mut output =
        String::from("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    output.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    output.push_str("<title>");
    output.push_str(&html_escape(&document.title));
    output.push_str("</title>\n</head>\n<body>\n<article>\n");
    if !document.title.is_empty() {
        output.push_str("<h1>");
        output.push_str(&html_escape(&document.title));
        output.push_str("</h1>\n");
    }
    for block in &document.block_order {
        match *block {
            DocumentBlock::Paragraph(id) => {
                let paragraph = document
                    .paragraphs
                    .iter()
                    .find(|paragraph| paragraph.id == id)
                    .ok_or_else(|| invalid_data("document block references a missing paragraph"))?;
                let content = html_paragraph_text(paragraph);
                match paragraph.list_kind {
                    ListKind::Bullet => {
                        output.push_str("<ul><li>");
                        output.push_str(&content);
                        output.push_str("</li></ul>\n");
                    }
                    ListKind::Numbered => {
                        output.push_str("<ol><li>");
                        output.push_str(&content);
                        output.push_str("</li></ol>\n");
                    }
                    ListKind::None => {
                        let (tag, close) = match paragraph.style {
                            ParagraphStyle::Normal => ("p", "p"),
                            ParagraphStyle::Heading1 => ("h2", "h2"),
                            ParagraphStyle::Heading2 => ("h3", "h3"),
                            ParagraphStyle::Quote => ("blockquote", "blockquote"),
                            ParagraphStyle::Code => ("pre", "pre"),
                        };
                        output.push('<');
                        output.push_str(tag);
                        output.push('>');
                        output.push_str(&content);
                        output.push_str("</");
                        output.push_str(close);
                        output.push_str(">\n");
                    }
                }
            }
            DocumentBlock::Table(id) => {
                let table = document
                    .tables
                    .iter()
                    .find(|table| table.id == id)
                    .ok_or_else(|| invalid_data("document block references a missing table"))?;
                output.push_str("<table>\n<tbody>\n");
                for row in &table.rows {
                    output.push_str("<tr>");
                    for cell in &row.cells {
                        output.push_str("<td>");
                        output.push_str(&html_escape(&cell.text));
                        output.push_str("</td>");
                    }
                    output.push_str("</tr>\n");
                }
                output.push_str("</tbody>\n</table>\n");
            }
        }
    }
    output.push_str("</article>\n</body>\n</html>\n");
    Ok(output)
}

fn html_paragraph_text(paragraph: &Paragraph) -> String {
    if paragraph.spans.is_empty() {
        return html_escape(&paragraph.text);
    }
    let mut boundaries = vec![0, paragraph.text.len()];
    for span in &paragraph.spans {
        boundaries.push(span.start);
        boundaries.push(span.end);
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut output = String::new();
    for pair in boundaries.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if start == end
            || !paragraph.text.is_char_boundary(start)
            || !paragraph.text.is_char_boundary(end)
        {
            continue;
        }
        let mut segment = html_escape(&paragraph.text[start..end]);
        let active: Vec<InlineMark> = paragraph
            .spans
            .iter()
            .filter(|span| span.start <= start && span.end >= end)
            .map(|span| span.mark)
            .collect();
        if active.contains(&InlineMark::Bold) {
            segment = format!("<strong>{segment}</strong>");
        }
        if active.contains(&InlineMark::Italic) {
            segment = format!("<em>{segment}</em>");
        }
        if active.contains(&InlineMark::Underline) {
            segment = format!("<u>{segment}</u>");
        }
        output.push_str(&segment);
    }
    output
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn markdown_paragraph_text(paragraph: &Paragraph) -> String {
    if paragraph.spans.is_empty() {
        return markdown_escape(&paragraph.text);
    }
    let mut boundaries = vec![0, paragraph.text.len()];
    for span in &paragraph.spans {
        boundaries.push(span.start);
        boundaries.push(span.end);
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut output = String::new();
    for pair in boundaries.windows(2) {
        let start = pair[0];
        let end = pair[1];
        if start == end
            || !paragraph.text.is_char_boundary(start)
            || !paragraph.text.is_char_boundary(end)
        {
            continue;
        }
        let segment = markdown_escape(&paragraph.text[start..end]);
        let active: Vec<InlineMark> = paragraph
            .spans
            .iter()
            .filter(|span| span.start <= start && span.end >= end)
            .map(|span| span.mark)
            .collect();
        let mut rendered = segment;
        if active.contains(&InlineMark::Bold) {
            rendered = format!("**{rendered}**");
        }
        if active.contains(&InlineMark::Italic) {
            rendered = format!("*{rendered}*");
        }
        if active.contains(&InlineMark::Underline) {
            rendered = format!("<u>{rendered}</u>");
        }
        output.push_str(&rendered);
    }
    output
}

fn markdown_escape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(
            character,
            '\\' | '`'
                | '*'
                | '_'
                | '{'
                | '}'
                | '['
                | ']'
                | '<'
                | '>'
                | '('
                | ')'
                | '#'
                | '+'
                | '-'
                | '.'
                | '!'
                | '|'
        ) {
            output.push('\\');
        }
        output.push(character);
    }
    output
}

fn markdown_table_escape(value: &str) -> String {
    markdown_escape(&value.replace('\n', "<br>"))
}

pub fn encode_document(document: &Document) -> io::Result<Vec<u8>> {
    validate_document_size(document)?;
    let title_len = u32::try_from(document.title.len())
        .map_err(|_| invalid_input("document title length is not representable"))?;
    let body_len = u64::try_from(document.body.len())
        .map_err(|_| invalid_input("document body length is not representable"))?;

    let mut bytes =
        Vec::with_capacity(DOCUMENT_HEADER_SIZE + document.title.len() + document.body.len());
    bytes.extend_from_slice(DOCUMENT_MAGIC);
    bytes.extend_from_slice(&DOCUMENT_FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&document.revision.to_le_bytes());
    bytes.extend_from_slice(&title_len.to_le_bytes());
    bytes.extend_from_slice(&body_len.to_le_bytes());
    bytes.extend_from_slice(document.title.as_bytes());
    bytes.extend_from_slice(document.body.as_bytes());
    let paragraph_count = u32::try_from(document.paragraphs.len())
        .map_err(|_| invalid_input("paragraph count is not representable"))?;
    bytes.extend_from_slice(&paragraph_count.to_le_bytes());
    bytes.extend_from_slice(&document.next_block_id.to_le_bytes());
    for paragraph in &document.paragraphs {
        let text_len = u64::try_from(paragraph.text.len())
            .map_err(|_| invalid_input("paragraph text length is not representable"))?;
        bytes.extend_from_slice(&paragraph.id.to_le_bytes());
        bytes.push(paragraph.style as u8);
        bytes.push(paragraph.list_kind as u8);
        bytes.push(paragraph.list_level);
        bytes.extend_from_slice(&text_len.to_le_bytes());
        bytes.extend_from_slice(paragraph.text.as_bytes());
        let span_count = u32::try_from(paragraph.spans.len())
            .map_err(|_| invalid_input("inline span count is not representable"))?;
        bytes.extend_from_slice(&span_count.to_le_bytes());
        for span in &paragraph.spans {
            bytes.extend_from_slice(&(span.start as u64).to_le_bytes());
            bytes.extend_from_slice(&(span.end as u64).to_le_bytes());
            bytes.push(span.mark as u8);
        }
    }
    let table_count = u32::try_from(document.tables.len())
        .map_err(|_| invalid_input("table count is not representable"))?;
    bytes.extend_from_slice(&table_count.to_le_bytes());
    for table in &document.tables {
        bytes.extend_from_slice(&table.id.to_le_bytes());
        bytes.extend_from_slice(&(table.rows.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(table.columns as u32).to_le_bytes());
        for row in &table.rows {
            bytes.extend_from_slice(&row.id.to_le_bytes());
            for cell in &row.cells {
                bytes.extend_from_slice(&cell.id.to_le_bytes());
                bytes.extend_from_slice(&(cell.text.len() as u64).to_le_bytes());
                bytes.extend_from_slice(cell.text.as_bytes());
            }
        }
    }
    let block_count = u32::try_from(document.block_order.len())
        .map_err(|_| invalid_input("block count is not representable"))?;
    bytes.extend_from_slice(&block_count.to_le_bytes());
    for block in &document.block_order {
        match block {
            DocumentBlock::Paragraph(id) => {
                bytes.push(0);
                bytes.extend_from_slice(&id.to_le_bytes());
            }
            DocumentBlock::Table(id) => {
                bytes.push(1);
                bytes.extend_from_slice(&id.to_le_bytes());
            }
        }
    }
    let formatting_table_count = u32::try_from(document.tables.len())
        .map_err(|_| invalid_input("table formatting count is not representable"))?;
    bytes.extend_from_slice(&formatting_table_count.to_le_bytes());
    for table in &document.tables {
        bytes.extend_from_slice(&table.id.to_le_bytes());
        for width in &table.column_widths_twips {
            bytes.extend_from_slice(&width.to_le_bytes());
        }
        for row in &table.rows {
            bytes.extend_from_slice(&row.id.to_le_bytes());
            bytes.extend_from_slice(&row.height_twips.to_le_bytes());
            for cell in &row.cells {
                bytes.extend_from_slice(&cell.id.to_le_bytes());
                bytes.push(cell.alignment as u8);
                bytes.extend_from_slice(&cell.background.unwrap_or(u32::MAX).to_le_bytes());
                bytes.push(cell.borders);
            }
        }
    }
    if bytes.len() > MAX_DOCUMENT_SIZE {
        return Err(invalid_input(
            "structured document exceeds the recovery-record limit",
        ));
    }
    Ok(bytes)
}

/// Decode a complete document, rejecting unknown versions and malformed UTF-8.
pub fn decode_document(bytes: &[u8]) -> io::Result<Document> {
    if bytes.len() < DOCUMENT_HEADER_SIZE || &bytes[..8] != DOCUMENT_MAGIC {
        return Err(invalid_data("invalid document header"));
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if version != DOCUMENT_FORMAT_VERSION
        && version != PREVIOUS_DOCUMENT_FORMAT_VERSION
        && version != LEGACY_TABLE_FORMAT_VERSION
        && version != INLINE_MARK_FORMAT_VERSION
        && version != LEGACY_LIST_FORMAT_VERSION
        && version != LEGACY_STRUCTURED_FORMAT_VERSION
        && version != LEGACY_DOCUMENT_FORMAT_VERSION
    {
        return Err(invalid_data("unsupported document format version"));
    }
    let revision = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
    let title_len = u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;
    let body_len = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    let body_len = usize::try_from(body_len)
        .map_err(|_| invalid_data("document body length is not representable"))?;
    let text_end = DOCUMENT_HEADER_SIZE
        .checked_add(title_len)
        .and_then(|len| len.checked_add(body_len))
        .ok_or_else(|| invalid_data("document size overflow"))?;
    if text_end > bytes.len() || text_end > MAX_DOCUMENT_SIZE || title_len > MAX_TITLE_SIZE {
        return Err(invalid_data("document length is invalid"));
    }
    let title_end = DOCUMENT_HEADER_SIZE + title_len;
    let title = std::str::from_utf8(&bytes[DOCUMENT_HEADER_SIZE..title_end])
        .map_err(|_| invalid_data("document title is not valid UTF-8"))?;
    let body = std::str::from_utf8(&bytes[title_end..text_end])
        .map_err(|_| invalid_data("document body is not valid UTF-8"))?;

    let mut tables = Vec::new();
    let (paragraphs, next_block_id, block_order) = if version == LEGACY_DOCUMENT_FORMAT_VERSION {
        if text_end != bytes.len() {
            return Err(invalid_data("legacy document contains trailing data"));
        }
        let mut paragraphs = Vec::new();
        for (index, text) in body.split('\n').enumerate() {
            let id = u64::try_from(index + 1)
                .map_err(|_| invalid_data("legacy paragraph ID overflow"))?;
            paragraphs.push(Paragraph {
                id,
                text: text.to_owned(),
                style: ParagraphStyle::Normal,
                list_kind: ListKind::None,
                list_level: 0,
                spans: Vec::new(),
            });
        }
        let next_id = u64::try_from(paragraphs.len())
            .ok()
            .and_then(|count| count.checked_add(1))
            .ok_or_else(|| invalid_data("legacy paragraph ID overflow"))?;
        let order = paragraphs
            .iter()
            .map(|p| DocumentBlock::Paragraph(p.id))
            .collect();
        (paragraphs, next_id, order)
    } else {
        let mut cursor = text_end;
        let count_end = cursor
            .checked_add(4)
            .ok_or_else(|| invalid_data("paragraph metadata overflow"))?;
        if count_end > bytes.len() {
            return Err(invalid_data("truncated paragraph metadata"));
        }
        let count = u32::from_le_bytes(bytes[cursor..count_end].try_into().unwrap()) as usize;
        cursor = count_end;
        let next_end = cursor
            .checked_add(8)
            .ok_or_else(|| invalid_data("paragraph metadata overflow"))?;
        if next_end > bytes.len() {
            return Err(invalid_data("truncated next paragraph ID"));
        }
        let next_id = u64::from_le_bytes(bytes[cursor..next_end].try_into().unwrap());
        cursor = next_end;
        if count == 0
            || count > body_len.saturating_add(1)
            || count
                > bytes.len().saturating_sub(cursor)
                    / if version >= LEGACY_LIST_FORMAT_VERSION {
                        19
                    } else {
                        17
                    }
        {
            return Err(invalid_data("paragraph count is invalid"));
        }
        let mut paragraphs = Vec::with_capacity(count);
        let mut ids = std::collections::HashSet::with_capacity(count);
        for _ in 0..count {
            let fixed_size = if version >= LEGACY_LIST_FORMAT_VERSION {
                19
            } else {
                17
            };
            let fixed_end = cursor
                .checked_add(fixed_size)
                .ok_or_else(|| invalid_data("paragraph metadata overflow"))?;
            if fixed_end > bytes.len() {
                return Err(invalid_data("truncated paragraph record"));
            }
            let id = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
            let style = ParagraphStyle::from_byte(bytes[cursor + 8])?;
            let (list_kind, list_level, length_offset) = if version >= LEGACY_LIST_FORMAT_VERSION {
                (
                    ListKind::from_byte(bytes[cursor + 9])?,
                    bytes[cursor + 10],
                    cursor + 11,
                )
            } else {
                (ListKind::None, 0, cursor + 9)
            };
            let text_len =
                u64::from_le_bytes(bytes[length_offset..length_offset + 8].try_into().unwrap());
            cursor = fixed_end;
            let text_len = usize::try_from(text_len)
                .map_err(|_| invalid_data("paragraph text length overflow"))?;
            let paragraph_end = cursor
                .checked_add(text_len)
                .ok_or_else(|| invalid_data("paragraph text length overflow"))?;
            if id == 0 || !ids.insert(id) || paragraph_end > bytes.len() {
                return Err(invalid_data("invalid paragraph ID or text length"));
            }
            let text = std::str::from_utf8(&bytes[cursor..paragraph_end])
                .map_err(|_| invalid_data("paragraph text is not valid UTF-8"))?;
            cursor = paragraph_end;
            let mut spans = Vec::new();
            if version >= INLINE_MARK_FORMAT_VERSION {
                let count_end = cursor
                    .checked_add(4)
                    .ok_or_else(|| invalid_data("inline span metadata overflow"))?;
                if count_end > bytes.len() {
                    return Err(invalid_data("truncated inline span count"));
                }
                let span_count =
                    u32::from_le_bytes(bytes[cursor..count_end].try_into().unwrap()) as usize;
                cursor = count_end;
                if span_count > bytes.len().saturating_sub(cursor) / 17 {
                    return Err(invalid_data("inline span count is invalid"));
                }
                spans.reserve(span_count);
                for _ in 0..span_count {
                    let span_end = cursor
                        .checked_add(17)
                        .ok_or_else(|| invalid_data("inline span metadata overflow"))?;
                    if span_end > bytes.len() {
                        return Err(invalid_data("truncated inline span"));
                    }
                    let start = usize::try_from(u64::from_le_bytes(
                        bytes[cursor..cursor + 8].try_into().unwrap(),
                    ))
                    .map_err(|_| invalid_data("inline span offset overflow"))?;
                    let end = usize::try_from(u64::from_le_bytes(
                        bytes[cursor + 8..cursor + 16].try_into().unwrap(),
                    ))
                    .map_err(|_| invalid_data("inline span offset overflow"))?;
                    let mark = InlineMark::from_byte(bytes[cursor + 16])?;
                    spans.push(TextSpan { start, end, mark });
                    cursor = span_end;
                }
            }
            paragraphs.push(Paragraph {
                id,
                text: text.to_owned(),
                style,
                list_kind,
                list_level,
                spans,
            });
        }
        if version >= LEGACY_TABLE_FORMAT_VERSION {
            let count_end = cursor
                .checked_add(4)
                .ok_or_else(|| invalid_data("table count overflow"))?;
            if count_end > bytes.len() {
                return Err(invalid_data("truncated table count"));
            }
            let table_count =
                u32::from_le_bytes(bytes[cursor..count_end].try_into().unwrap()) as usize;
            cursor = count_end;
            if table_count > bytes.len().saturating_sub(cursor) / 16 {
                return Err(invalid_data("table count is invalid"));
            }
            for _ in 0..table_count {
                let end = cursor
                    .checked_add(16)
                    .ok_or_else(|| invalid_data("table header overflow"))?;
                if end > bytes.len() {
                    return Err(invalid_data("truncated table header"));
                }
                let table_id = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
                let row_count =
                    u32::from_le_bytes(bytes[cursor + 8..cursor + 12].try_into().unwrap()) as usize;
                let columns =
                    u32::from_le_bytes(bytes[cursor + 12..end].try_into().unwrap()) as usize;
                cursor = end;
                if table_id == 0
                    || row_count == 0
                    || columns == 0
                    || row_count > 1024
                    || columns > 1024
                    || row_count.checked_mul(columns).is_none_or(|n| n > 65_536)
                {
                    return Err(invalid_data("table dimensions or ID are invalid"));
                }
                let mut rows = Vec::with_capacity(row_count);
                for _ in 0..row_count {
                    let row_end = cursor
                        .checked_add(8)
                        .ok_or_else(|| invalid_data("row header overflow"))?;
                    if row_end > bytes.len() {
                        return Err(invalid_data("truncated table row"));
                    }
                    let row_id = u64::from_le_bytes(bytes[cursor..row_end].try_into().unwrap());
                    cursor = row_end;
                    let mut cells = Vec::with_capacity(columns);
                    for _ in 0..columns {
                        let cell_end = cursor
                            .checked_add(16)
                            .ok_or_else(|| invalid_data("cell header overflow"))?;
                        if cell_end > bytes.len() {
                            return Err(invalid_data("truncated table cell"));
                        }
                        let cell_id =
                            u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
                        let text_len = usize::try_from(u64::from_le_bytes(
                            bytes[cursor + 8..cell_end].try_into().unwrap(),
                        ))
                        .map_err(|_| invalid_data("cell text length overflow"))?;
                        cursor = cell_end;
                        let text_end = cursor
                            .checked_add(text_len)
                            .ok_or_else(|| invalid_data("cell text length overflow"))?;
                        if text_end > bytes.len() {
                            return Err(invalid_data("truncated cell text"));
                        }
                        let text = std::str::from_utf8(&bytes[cursor..text_end])
                            .map_err(|_| invalid_data("table cell text is not valid UTF-8"))?;
                        cursor = text_end;
                        if cell_id == 0 || !ids.insert(cell_id) {
                            return Err(invalid_data("duplicate or zero block ID"));
                        }
                        cells.push(TableCell {
                            id: cell_id,
                            text: text.to_owned(),
                            alignment: CellAlignment::Left,
                            background: None,
                            borders: 0x0f,
                        });
                    }
                    if row_id == 0 || !ids.insert(row_id) {
                        return Err(invalid_data("duplicate or zero block ID"));
                    }
                    rows.push(TableRow {
                        id: row_id,
                        cells,
                        height_twips: 0,
                    });
                }
                if !ids.insert(table_id) {
                    return Err(invalid_data("duplicate table ID"));
                }
                tables.push(Table {
                    id: table_id,
                    rows,
                    columns,
                    column_widths_twips: vec![2400; columns],
                });
            }
        }
        let order = if version >= BLOCK_ORDER_FORMAT_VERSION {
            let count_end = cursor
                .checked_add(4)
                .ok_or_else(|| invalid_data("block order count overflow"))?;
            if count_end > bytes.len() {
                return Err(invalid_data("truncated block order count"));
            }
            let count = u32::from_le_bytes(bytes[cursor..count_end].try_into().unwrap()) as usize;
            cursor = count_end;
            if count > bytes.len().saturating_sub(cursor) / 9 {
                return Err(invalid_data("block order count is invalid"));
            }
            let mut order = Vec::with_capacity(count);
            for _ in 0..count {
                let end = cursor
                    .checked_add(9)
                    .ok_or_else(|| invalid_data("block order overflow"))?;
                if end > bytes.len() {
                    return Err(invalid_data("truncated block order entry"));
                }
                let id = u64::from_le_bytes(bytes[cursor + 1..end].try_into().unwrap());
                let block = match bytes[cursor] {
                    0 => DocumentBlock::Paragraph(id),
                    1 => DocumentBlock::Table(id),
                    _ => return Err(invalid_data("unknown block order kind")),
                };
                order.push(block);
                cursor = end;
            }
            order
        } else {
            paragraphs
                .iter()
                .map(|p| DocumentBlock::Paragraph(p.id))
                .chain(tables.iter().map(|t| DocumentBlock::Table(t.id)))
                .collect()
        };
        if version >= TABLE_FORMATTING_FORMAT_VERSION {
            let count_end = cursor
                .checked_add(4)
                .ok_or_else(|| invalid_data("table formatting count overflow"))?;
            if count_end > bytes.len() {
                return Err(invalid_data("truncated table formatting count"));
            }
            let formatting_count =
                u32::from_le_bytes(bytes[cursor..count_end].try_into().unwrap()) as usize;
            cursor = count_end;
            if formatting_count != tables.len() {
                return Err(invalid_data("table formatting count does not match tables"));
            }
            for table in &mut tables {
                let table_end = cursor
                    .checked_add(8)
                    .ok_or_else(|| invalid_data("table formatting header overflow"))?;
                if table_end > bytes.len() {
                    return Err(invalid_data("truncated table formatting header"));
                }
                let table_id = u64::from_le_bytes(bytes[cursor..table_end].try_into().unwrap());
                cursor = table_end;
                if table_id != table.id {
                    return Err(invalid_data("table formatting ID mismatch"));
                }
                for width in &mut table.column_widths_twips {
                    let end = cursor
                        .checked_add(4)
                        .ok_or_else(|| invalid_data("column width overflow"))?;
                    if end > bytes.len() {
                        return Err(invalid_data("truncated column width"));
                    }
                    *width = u32::from_le_bytes(bytes[cursor..end].try_into().unwrap());
                    if !(100..=100_000).contains(width) {
                        return Err(invalid_data("table column width is invalid"));
                    }
                    cursor = end;
                }
                for row in &mut table.rows {
                    let row_end = cursor
                        .checked_add(12)
                        .ok_or_else(|| invalid_data("row formatting overflow"))?;
                    if row_end > bytes.len() {
                        return Err(invalid_data("truncated row formatting"));
                    }
                    let row_id = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
                    let height = u32::from_le_bytes(bytes[cursor + 8..row_end].try_into().unwrap());
                    cursor = row_end;
                    if row_id != row.id || (height != 0 && !(100..=100_000).contains(&height)) {
                        return Err(invalid_data("table row formatting is invalid"));
                    }
                    row.height_twips = height;
                    for cell in &mut row.cells {
                        let end = cursor
                            .checked_add(14)
                            .ok_or_else(|| invalid_data("cell formatting overflow"))?;
                        if end > bytes.len() {
                            return Err(invalid_data("truncated cell formatting"));
                        }
                        let cell_id =
                            u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
                        let alignment = CellAlignment::from_byte(bytes[cursor + 8])?;
                        let background =
                            u32::from_le_bytes(bytes[cursor + 9..cursor + 13].try_into().unwrap());
                        let borders = bytes[cursor + 13];
                        cursor = end;
                        if cell_id != cell.id
                            || borders & !0x0f != 0
                            || (background != u32::MAX && background > 0x00ff_ffff)
                        {
                            return Err(invalid_data("table cell formatting is invalid"));
                        }
                        cell.alignment = alignment;
                        cell.background = (background != u32::MAX).then_some(background);
                        cell.borders = borders;
                    }
                }
            }
        }
        if cursor != bytes.len() || next_id == 0 || ids.iter().any(|id| *id >= next_id) {
            return Err(invalid_data("paragraph metadata is inconsistent"));
        }
        let reconstructed = paragraphs
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if reconstructed != body {
            return Err(invalid_data(
                "paragraphs do not match the plain-text document view",
            ));
        }
        (paragraphs, next_id, order)
    };

    let document = Document {
        title: title.to_owned(),
        body: body.to_owned(),
        revision,
        paragraphs,
        tables,
        block_order,
        next_block_id,
    };
    validate_document_size(&document).map_err(|_| invalid_data("document exceeds size limit"))?;
    Ok(document)
}

fn adjust_inline_spans_for_edit(
    spans: &[TextSpan],
    range: std::ops::Range<usize>,
    replacement_len: usize,
) -> Vec<TextSpan> {
    let mut adjusted = Vec::new();
    for span in spans {
        if span.end <= range.start {
            adjusted.push(span.clone());
        } else if span.start >= range.end {
            let start = range.start + replacement_len + (span.start - range.end);
            let end = range.start + replacement_len + (span.end - range.end);
            adjusted.push(TextSpan {
                start,
                end,
                mark: span.mark,
            });
        } else {
            if span.start < range.start {
                adjusted.push(TextSpan {
                    start: span.start,
                    end: range.start,
                    mark: span.mark,
                });
            }
            if span.start <= range.start && span.end >= range.end && replacement_len > 0 {
                adjusted.push(TextSpan {
                    start: range.start,
                    end: range.start + replacement_len,
                    mark: span.mark,
                });
            }
            if span.end > range.end {
                let start = range.start + replacement_len;
                let end = start + (span.end - range.end);
                adjusted.push(TextSpan {
                    start,
                    end,
                    mark: span.mark,
                });
            }
        }
    }
    adjusted.sort_by_key(|span| (span.mark, span.start, span.end));
    let mut merged: Vec<TextSpan> = Vec::with_capacity(adjusted.len());
    for span in adjusted {
        if let Some(previous) = merged.last_mut() {
            if previous.mark == span.mark && span.start <= previous.end {
                previous.end = previous.end.max(span.end);
                continue;
            }
        }
        merged.push(span);
    }
    merged
}

fn validate_document_size(document: &Document) -> io::Result<()> {
    let mut size = DOCUMENT_HEADER_SIZE
        .checked_add(document.title.len())
        .and_then(|len| len.checked_add(document.body.len()))
        .and_then(|len| len.checked_add(12))
        .ok_or_else(|| invalid_input("document size overflow"))?;
    if document.paragraphs.is_empty() {
        return Err(invalid_input(
            "a document must contain at least one paragraph",
        ));
    }
    let mut ids = std::collections::HashSet::with_capacity(document.paragraphs.len());
    let mut max_id = 0;
    for paragraph in &document.paragraphs {
        if paragraph.id == 0 || !ids.insert(paragraph.id) {
            return Err(invalid_input("paragraph IDs must be unique and nonzero"));
        }
        if paragraph.text.contains('\n') {
            return Err(invalid_input(
                "paragraph text cannot contain line-feed characters",
            ));
        }
        if paragraph.list_level > 8
            || (paragraph.list_kind == ListKind::None && paragraph.list_level != 0)
        {
            return Err(invalid_input("paragraph list level is invalid"));
        }
        max_id = max_id.max(paragraph.id);
        let span_bytes = paragraph
            .spans
            .len()
            .checked_mul(17)
            .and_then(|len| len.checked_add(4))
            .ok_or_else(|| invalid_input("inline span size overflow"))?;
        let mut previous_span: Option<&TextSpan> = None;
        for span in &paragraph.spans {
            if span.start >= span.end
                || span.end > paragraph.text.len()
                || !paragraph.text.is_char_boundary(span.start)
                || !paragraph.text.is_char_boundary(span.end)
            {
                return Err(invalid_input("inline span range is invalid"));
            }
            if let Some(previous) = previous_span {
                if (span.mark, span.start, span.end)
                    <= (previous.mark, previous.start, previous.end)
                    || (span.mark == previous.mark && span.start < previous.end)
                {
                    return Err(invalid_input("inline spans are not canonical"));
                }
            }
            previous_span = Some(span);
        }
        size = size
            .checked_add(19)
            .and_then(|len| len.checked_add(span_bytes))
            .and_then(|len| len.checked_add(paragraph.text.len()))
            .ok_or_else(|| invalid_input("document size overflow"))?;
    }
    for table in &document.tables {
        if table.column_widths_twips.len() != table.columns
            || table
                .column_widths_twips
                .iter()
                .any(|width| !(100..=100_000).contains(width))
            || table.id == 0
            || !ids.insert(table.id)
            || table.rows.is_empty()
            || table.columns == 0
            || table.rows.len() > 1024
            || table.columns > 1024
            || table
                .rows
                .len()
                .checked_mul(table.columns)
                .is_none_or(|n| n > 65_536)
        {
            return Err(invalid_input("table dimensions or ID are invalid"));
        }
        max_id = max_id.max(table.id);
        for row in &table.rows {
            if row.height_twips != 0 && !(100..=100_000).contains(&row.height_twips) {
                return Err(invalid_input("table row height is invalid"));
            }
            if row.id == 0 || !ids.insert(row.id) || row.cells.len() != table.columns {
                return Err(invalid_input("table row ID or cell count is invalid"));
            }
            max_id = max_id.max(row.id);
            for cell in &row.cells {
                if cell.background.is_some_and(|color| color > 0x00ff_ffff)
                    || cell.borders & !0x0f != 0
                {
                    return Err(invalid_input("table cell formatting is invalid"));
                }
                if cell.id == 0 || !ids.insert(cell.id) {
                    return Err(invalid_input("table cell ID is invalid"));
                }
                max_id = max_id.max(cell.id);
                size = size
                    .checked_add(16)
                    .and_then(|n| n.checked_add(cell.text.len()))
                    .ok_or_else(|| invalid_input("table size overflow"))?;
            }
            size = size
                .checked_add(8)
                .ok_or_else(|| invalid_input("table size overflow"))?;
        }
        size = size
            .checked_add(16)
            .ok_or_else(|| invalid_input("table size overflow"))?;
    }
    size = size
        .checked_add(8)
        .and_then(|n| n.checked_add(document.block_order.len().checked_mul(9)?))
        .ok_or_else(|| invalid_input("block metadata size overflow"))?;
    for table in &document.tables {
        let table_format_bytes = table
            .columns
            .checked_mul(4)
            .and_then(|n| {
                table
                    .rows
                    .len()
                    .checked_mul(table.columns)?
                    .checked_mul(14)?
                    .checked_add(n)
            })
            .and_then(|n| table.rows.len().checked_mul(12)?.checked_add(n))
            .and_then(|n| n.checked_add(8))
            .ok_or_else(|| invalid_input("table formatting size overflow"))?;
        size = size
            .checked_add(table_format_bytes)
            .ok_or_else(|| invalid_input("table formatting size overflow"))?;
    }
    let expected_blocks = document.paragraphs.len() + document.tables.len();
    if document.block_order.len() != expected_blocks {
        return Err(invalid_input(
            "block order must reference every paragraph and table exactly once",
        ));
    }
    let mut ordered_ids = std::collections::HashSet::with_capacity(expected_blocks);
    for block in &document.block_order {
        let id = match block {
            DocumentBlock::Paragraph(id) if document.paragraphs.iter().any(|p| p.id == *id) => *id,
            DocumentBlock::Table(id) if document.tables.iter().any(|t| t.id == *id) => *id,
            _ => return Err(invalid_input("block order references an unknown block")),
        };
        if !ordered_ids.insert(id) {
            return Err(invalid_input("block order contains a duplicate block"));
        }
    }
    if document.next_block_id <= max_id {
        return Err(invalid_input(
            "next paragraph ID must exceed every existing ID",
        ));
    }
    let reconstructed = document
        .paragraphs
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if reconstructed != document.body {
        return Err(invalid_input(
            "paragraphs do not match the plain-text document view",
        ));
    }
    if size > MAX_DOCUMENT_SIZE {
        return Err(invalid_input(
            "document exceeds the 64 MiB recovery-record limit",
        ));
    }
    Ok(())
}

/// Owns one document and coordinates its journal, checkpoints and autosave.
pub struct DocumentSession {
    filesystem: LocalFileSystem,
    directory: PathBuf,
    document: Document,
    journal: RecoveryJournal,
    autosave: AutosaveScheduler,
    checkpoint_generation: u64,
    recovered_records: usize,
    journal_had_truncated_tail: bool,
}

impl DocumentSession {
    /// Open an existing document session or create a new empty one.
    ///
    /// The directory is local to this process and should have a single owner.
    pub fn open(directory: impl AsRef<Path>) -> io::Result<Self> {
        let directory = directory.as_ref().to_path_buf();
        let filesystem = LocalFileSystem;
        filesystem.create_dir_all(&directory)?;
        let checkpoint_path = directory.join(CHECKPOINT_FILE);
        let journal = RecoveryJournal::open(directory.join(JOURNAL_FILE))?;

        let checkpoint = if filesystem.exists(&checkpoint_path) {
            Some(read_checkpoint(&filesystem, &checkpoint_path)?)
        } else {
            None
        };

        let (mut document, checkpoint_generation, replay) = if let Some(checkpoint) = checkpoint {
            let replay = journal.replay_after(checkpoint.journal_sequence)?;
            (
                decode_document(&checkpoint.payload)?,
                checkpoint.generation,
                replay,
            )
        } else {
            let replay = journal.replay()?;
            if replay.base_sequence != 0 {
                return Err(invalid_data(
                    "journal has a checkpoint base sequence but the checkpoint is missing",
                ));
            }
            (Document::new("Untitled")?, 0, replay)
        };

        let recovered_records = replay.records.len();
        let journal_had_truncated_tail = replay.had_truncated_tail;
        for record in replay.records {
            let recovered = decode_document(&record.payload)?;
            let expected_revision = document
                .revision
                .checked_add(1)
                .ok_or_else(|| invalid_data("document revision exhausted during recovery"))?;
            if recovered.revision != expected_revision {
                return Err(invalid_data(
                    "recovery journal document revisions are not contiguous",
                ));
            }
            document = recovered;
        }

        let mut autosave = AutosaveScheduler::default();
        if recovered_records > 0 {
            // Recovered journal entries are safe on disk, but should be folded
            // into a checkpoint on the next autosave.
            autosave.mark_dirty(Instant::now());
        }

        Ok(Self {
            filesystem,
            directory,
            document,
            journal,
            autosave,
            checkpoint_generation,
            recovered_records,
            journal_had_truncated_tail,
        })
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    pub fn recovered_records(&self) -> usize {
        self.recovered_records
    }

    pub fn journal_had_truncated_tail(&self) -> bool {
        self.journal_had_truncated_tail
    }

    /// Insert an empty rectangular table. Returns its stable table ID.
    pub fn insert_table(&mut self, rows: usize, columns: usize) -> io::Result<u64> {
        if rows == 0
            || columns == 0
            || rows > 1024
            || columns > 1024
            || rows.checked_mul(columns).is_none_or(|n| n > 65_536)
        {
            return Err(invalid_input("table dimensions exceed supported limits"));
        }
        let mut next_id = self.document.next_block_id;
        let mut take_id = || -> io::Result<u64> {
            let id = next_id;
            next_id = next_id
                .checked_add(1)
                .ok_or_else(|| invalid_input("block ID space exhausted"))?;
            Ok(id)
        };
        let table_id = take_id()?;
        let mut table_rows = Vec::with_capacity(rows);
        for _ in 0..rows {
            let row_id = take_id()?;
            let mut cells = Vec::with_capacity(columns);
            for _ in 0..columns {
                cells.push(TableCell {
                    id: take_id()?,
                    text: String::new(),
                    alignment: CellAlignment::Left,
                    background: None,
                    borders: 0x0f,
                });
            }
            table_rows.push(TableRow {
                id: row_id,
                cells,
                height_twips: 0,
            });
        }
        let mut next = self.document.clone();
        self.document.increment_revision(&mut next)?;
        next.tables.push(Table {
            id: table_id,
            rows: table_rows,
            columns,
            column_widths_twips: vec![2400; columns],
        });
        next.block_order.push(DocumentBlock::Table(table_id));
        next.next_block_id = next_id;
        validate_document_size(&next)?;
        self.record_change(next)?;
        Ok(table_id)
    }

    /// Insert an empty row before the given zero-based row index.
    /// Existing row and cell IDs remain stable; only newly created objects get new IDs.
    pub fn insert_table_row(&mut self, table_id: u64, row_index: usize) -> io::Result<u64> {
        let mut next = self.document.clone();
        let next_id = next.next_block_id;
        let table = next
            .tables
            .iter_mut()
            .find(|table| table.id == table_id)
            .ok_or_else(|| invalid_input("table ID does not exist"))?;
        if row_index > table.rows.len()
            || table.rows.len() >= 1024
            || table
                .rows
                .len()
                .checked_add(1)
                .and_then(|n| n.checked_mul(table.columns))
                .is_none_or(|n| n > 65_536)
        {
            return Err(invalid_input(
                "table row insertion exceeds supported limits",
            ));
        }
        let mut id = next_id;
        let row_id = id;
        id = id
            .checked_add(1)
            .ok_or_else(|| invalid_input("block ID space exhausted"))?;
        let mut cells = Vec::with_capacity(table.columns);
        for _ in 0..table.columns {
            let cell_id = id;
            id = id
                .checked_add(1)
                .ok_or_else(|| invalid_input("block ID space exhausted"))?;
            cells.push(TableCell {
                id: cell_id,
                text: String::new(),
                alignment: CellAlignment::Left,
                background: None,
                borders: 0x0f,
            });
        }
        table.rows.insert(
            row_index,
            TableRow {
                id: row_id,
                cells,
                height_twips: 0,
            },
        );
        next.next_block_id = id;
        self.document.increment_revision(&mut next)?;
        validate_document_size(&next)?;
        self.record_change(next)?;
        Ok(row_id)
    }

    /// Remove a row by zero-based index. A table always retains at least one row.
    pub fn delete_table_row(&mut self, table_id: u64, row_index: usize) -> io::Result<u64> {
        let mut next = self.document.clone();
        let table = next
            .tables
            .iter_mut()
            .find(|table| table.id == table_id)
            .ok_or_else(|| invalid_input("table ID does not exist"))?;
        if table.rows.len() <= 1 || row_index >= table.rows.len() {
            return Err(invalid_input("cannot remove the requested table row"));
        }
        table.rows.remove(row_index);
        self.document.increment_revision(&mut next)?;
        validate_document_size(&next)?;
        self.record_change(next)
    }

    /// Insert an empty column before the given zero-based column index.
    /// Existing row and cell IDs remain stable.
    pub fn insert_table_column(&mut self, table_id: u64, column_index: usize) -> io::Result<u64> {
        let mut next = self.document.clone();
        let mut id = next.next_block_id;
        let table = next
            .tables
            .iter_mut()
            .find(|table| table.id == table_id)
            .ok_or_else(|| invalid_input("table ID does not exist"))?;
        if column_index > table.columns
            || table.columns >= 1024
            || table
                .rows
                .len()
                .checked_mul(table.columns + 1)
                .is_none_or(|n| n > 65_536)
        {
            return Err(invalid_input(
                "table column insertion exceeds supported limits",
            ));
        }
        let first_cell_id = id;
        for row in &mut table.rows {
            let cell_id = id;
            id = id
                .checked_add(1)
                .ok_or_else(|| invalid_input("block ID space exhausted"))?;
            row.cells.insert(
                column_index,
                TableCell {
                    id: cell_id,
                    text: String::new(),
                    alignment: CellAlignment::Left,
                    background: None,
                    borders: 0x0f,
                },
            );
        }
        table.columns += 1;
        table.column_widths_twips.insert(column_index, 2400);
        next.next_block_id = id;
        self.document.increment_revision(&mut next)?;
        validate_document_size(&next)?;
        self.record_change(next)?;
        Ok(first_cell_id)
    }

    /// Remove a column by zero-based index. A table always retains at least one column.
    pub fn delete_table_column(&mut self, table_id: u64, column_index: usize) -> io::Result<u64> {
        let mut next = self.document.clone();
        let table = next
            .tables
            .iter_mut()
            .find(|table| table.id == table_id)
            .ok_or_else(|| invalid_input("table ID does not exist"))?;
        if table.columns <= 1 || column_index >= table.columns {
            return Err(invalid_input("cannot remove the requested table column"));
        }
        for row in &mut table.rows {
            row.cells.remove(column_index);
        }
        table.columns -= 1;
        table.column_widths_twips.remove(column_index);
        self.document.increment_revision(&mut next)?;
        validate_document_size(&next)?;
        self.record_change(next)
    }

    /// Set a column width in twips (1/20 point), limited to 5..5000 points.
    pub fn set_table_column_width(
        &mut self,
        table_id: u64,
        column_index: usize,
        width_twips: u32,
    ) -> io::Result<u64> {
        if !(100..=100_000).contains(&width_twips) {
            return Err(invalid_input("table column width is out of range"));
        }
        let mut next = self.document.clone();
        let table = next
            .tables
            .iter_mut()
            .find(|table| table.id == table_id)
            .ok_or_else(|| invalid_input("table ID does not exist"))?;
        let width = table
            .column_widths_twips
            .get_mut(column_index)
            .ok_or_else(|| invalid_input("table column index is out of range"))?;
        *width = width_twips;
        self.document.increment_revision(&mut next)?;
        validate_document_size(&next)?;
        self.record_change(next)
    }

    /// Set a row height in twips; zero restores automatic height.
    pub fn set_table_row_height(
        &mut self,
        table_id: u64,
        row_index: usize,
        height_twips: u32,
    ) -> io::Result<u64> {
        if height_twips != 0 && !(100..=100_000).contains(&height_twips) {
            return Err(invalid_input("table row height is out of range"));
        }
        let mut next = self.document.clone();
        let table = next
            .tables
            .iter_mut()
            .find(|table| table.id == table_id)
            .ok_or_else(|| invalid_input("table ID does not exist"))?;
        let row = table
            .rows
            .get_mut(row_index)
            .ok_or_else(|| invalid_input("table row index is out of range"))?;
        row.height_twips = height_twips;
        self.document.increment_revision(&mut next)?;
        validate_document_size(&next)?;
        self.record_change(next)
    }

    /// Set horizontal alignment for one cell.
    pub fn set_table_cell_alignment(
        &mut self,
        table_id: u64,
        row_index: usize,
        column_index: usize,
        alignment: CellAlignment,
    ) -> io::Result<u64> {
        let mut next = self.document.clone();
        let table = next
            .tables
            .iter_mut()
            .find(|table| table.id == table_id)
            .ok_or_else(|| invalid_input("table ID does not exist"))?;
        let cell = table
            .rows
            .get_mut(row_index)
            .and_then(|row| row.cells.get_mut(column_index))
            .ok_or_else(|| invalid_input("table cell coordinates are out of range"))?;
        cell.alignment = alignment;
        self.document.increment_revision(&mut next)?;
        validate_document_size(&next)?;
        self.record_change(next)
    }

    /// Set an optional 24-bit RGB cell background (0xRRGGBB).
    pub fn set_table_cell_background(
        &mut self,
        table_id: u64,
        row_index: usize,
        column_index: usize,
        background: Option<u32>,
    ) -> io::Result<u64> {
        if background.is_some_and(|color| color > 0x00ff_ffff) {
            return Err(invalid_input(
                "table cell background must be a 24-bit RGB color",
            ));
        }
        let mut next = self.document.clone();
        let table = next
            .tables
            .iter_mut()
            .find(|table| table.id == table_id)
            .ok_or_else(|| invalid_input("table ID does not exist"))?;
        let cell = table
            .rows
            .get_mut(row_index)
            .and_then(|row| row.cells.get_mut(column_index))
            .ok_or_else(|| invalid_input("table cell coordinates are out of range"))?;
        cell.background = background;
        self.document.increment_revision(&mut next)?;
        validate_document_size(&next)?;
        self.record_change(next)
    }

    /// Set cell borders using a bit mask: top=1, right=2, bottom=4, left=8.
    pub fn set_table_cell_borders(
        &mut self,
        table_id: u64,
        row_index: usize,
        column_index: usize,
        borders: u8,
    ) -> io::Result<u64> {
        if borders & !0x0f != 0 {
            return Err(invalid_input("table cell border mask is invalid"));
        }
        let mut next = self.document.clone();
        let table = next
            .tables
            .iter_mut()
            .find(|table| table.id == table_id)
            .ok_or_else(|| invalid_input("table ID does not exist"))?;
        let cell = table
            .rows
            .get_mut(row_index)
            .and_then(|row| row.cells.get_mut(column_index))
            .ok_or_else(|| invalid_input("table cell coordinates are out of range"))?;
        cell.borders = borders;
        self.document.increment_revision(&mut next)?;
        validate_document_size(&next)?;
        self.record_change(next)
    }

    /// Set text in a table cell using zero-based row and column indices.
    pub fn set_table_cell_text(
        &mut self,
        table_id: u64,
        row_index: usize,
        column_index: usize,
        text: impl Into<String>,
    ) -> io::Result<u64> {
        let mut next = self.document.clone();
        let table = next
            .tables
            .iter_mut()
            .find(|table| table.id == table_id)
            .ok_or_else(|| invalid_input("table ID does not exist"))?;
        let cell = table
            .rows
            .get_mut(row_index)
            .and_then(|row| row.cells.get_mut(column_index))
            .ok_or_else(|| invalid_input("table cell coordinates are out of range"))?;
        cell.text = text.into();
        self.document.increment_revision(&mut next)?;
        validate_document_size(&next)?;
        self.record_change(next)
    }

    /// Change the title and durably journal the resulting document snapshot.
    pub fn set_title(&mut self, title: impl Into<String>) -> io::Result<u64> {
        let next = self.document.with_title(title.into())?;
        self.record_change(next)
    }

    /// Replace the body and durably journal the resulting document snapshot.
    pub fn set_body(&mut self, body: impl Into<String>) -> io::Result<u64> {
        let next = self.document.with_body(body.into())?;
        self.record_change(next)
    }

    /// Insert UTF-8 text at a byte offset that must be a character boundary.
    pub fn insert_text(&mut self, offset: usize, text: &str) -> io::Result<u64> {
        self.replace_range(offset..offset, text)
    }

    /// Delete a byte range whose endpoints must both be UTF-8 character boundaries.
    pub fn delete_range(&mut self, range: std::ops::Range<usize>) -> io::Result<u64> {
        self.replace_range(range, "")
    }

    /// Replace a byte range while preserving valid UTF-8.
    pub fn replace_range(
        &mut self,
        range: std::ops::Range<usize>,
        replacement: &str,
    ) -> io::Result<u64> {
        let mut body = self.document.body.clone();
        if range.start > range.end
            || range.end > body.len()
            || !body.is_char_boundary(range.start)
            || !body.is_char_boundary(range.end)
        {
            return Err(invalid_input("text range is not a valid UTF-8 byte range"));
        }
        let same_paragraph = !body[range.clone()].contains('\n') && !replacement.contains('\n');
        let paragraph_index = if same_paragraph {
            Some(
                body[..range.start]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count(),
            )
        } else {
            None
        };
        let paragraph_local_range = if let Some(index) = paragraph_index {
            let line_start = body[..range.start]
                .rfind('\n')
                .map(|offset| offset + 1)
                .unwrap_or(0);
            Some((index, range.start - line_start..range.end - line_start))
        } else {
            None
        };
        let old_spans = paragraph_index
            .and_then(|index| self.document.paragraphs.get(index))
            .map(|paragraph| paragraph.spans.clone());
        body.replace_range(range, replacement);
        let mut next = self.document.with_body(body)?;
        if let (Some((index, local_range)), Some(spans)) = (paragraph_local_range, old_spans) {
            if let Some(paragraph) = next.paragraphs.get_mut(index) {
                paragraph.spans =
                    adjust_inline_spans_for_edit(&spans, local_range, replacement.len());
            }
            validate_document_size(&next)?;
        }
        self.record_change(next)
    }

    /// Change semantic formatting for a paragraph without changing its stable ID.
    pub fn set_paragraph_style(
        &mut self,
        paragraph_id: u64,
        style: ParagraphStyle,
    ) -> io::Result<u64> {
        let mut paragraphs = self.document.paragraphs.clone();
        let paragraph = paragraphs
            .iter_mut()
            .find(|paragraph| paragraph.id == paragraph_id)
            .ok_or_else(|| invalid_input("paragraph ID does not exist"))?;
        paragraph.style = style;
        let next = self
            .document
            .with_paragraphs(paragraphs, self.document.next_block_id)?;
        self.record_change(next)
    }

    /// Apply or remove one inline mark over a UTF-8 byte range in a paragraph.
    /// Existing marks of other kinds are preserved; adjacent ranges are merged.
    pub fn set_inline_mark(
        &mut self,
        paragraph_id: u64,
        range: std::ops::Range<usize>,
        mark: InlineMark,
        enabled: bool,
    ) -> io::Result<u64> {
        let mut paragraphs = self.document.paragraphs.clone();
        let paragraph = paragraphs
            .iter_mut()
            .find(|paragraph| paragraph.id == paragraph_id)
            .ok_or_else(|| invalid_input("paragraph ID does not exist"))?;
        if range.start >= range.end
            || range.end > paragraph.text.len()
            || !paragraph.text.is_char_boundary(range.start)
            || !paragraph.text.is_char_boundary(range.end)
        {
            return Err(invalid_input("inline formatting range is invalid"));
        }
        let mut intervals: Vec<(usize, usize)> = paragraph
            .spans
            .iter()
            .filter(|span| span.mark == mark)
            .map(|span| (span.start, span.end))
            .collect();
        if enabled {
            intervals.push((range.start, range.end));
            intervals.sort_unstable();
            let mut merged: Vec<(usize, usize)> = Vec::new();
            for (start, end) in intervals {
                if let Some(last) = merged.last_mut() {
                    if start <= last.1 {
                        last.1 = last.1.max(end);
                        continue;
                    }
                }
                merged.push((start, end));
            }
            intervals = merged;
        } else {
            let mut remaining = Vec::new();
            for (start, end) in intervals {
                if end <= range.start || start >= range.end {
                    remaining.push((start, end));
                } else {
                    if start < range.start {
                        remaining.push((start, range.start));
                    }
                    if end > range.end {
                        remaining.push((range.end, end));
                    }
                }
            }
            intervals = remaining;
        }
        paragraph.spans.retain(|span| span.mark != mark);
        paragraph
            .spans
            .extend(
                intervals
                    .into_iter()
                    .map(|(start, end)| TextSpan { start, end, mark }),
            );
        paragraph
            .spans
            .sort_by_key(|span| (span.mark, span.start, span.end));
        let next = self
            .document
            .with_paragraphs(paragraphs, self.document.next_block_id)?;
        self.record_change(next)
    }

    /// Set bullet/numbered list semantics and nesting level for a paragraph.
    /// Level zero is the outermost list; levels above eight are rejected.
    pub fn set_paragraph_list(
        &mut self,
        paragraph_id: u64,
        kind: ListKind,
        level: u8,
    ) -> io::Result<u64> {
        if level > 8 || (kind == ListKind::None && level != 0) {
            return Err(invalid_input("paragraph list level is invalid"));
        }
        let mut paragraphs = self.document.paragraphs.clone();
        let paragraph = paragraphs
            .iter_mut()
            .find(|paragraph| paragraph.id == paragraph_id)
            .ok_or_else(|| invalid_input("paragraph ID does not exist"))?;
        paragraph.list_kind = kind;
        paragraph.list_level = level;
        let next = self
            .document
            .with_paragraphs(paragraphs, self.document.next_block_id)?;
        self.record_change(next)
    }

    /// Insert a paragraph after the supplied ID, or append when no ID is supplied.
    pub fn insert_paragraph_after(
        &mut self,
        after_id: Option<u64>,
        text: impl Into<String>,
        style: ParagraphStyle,
    ) -> io::Result<u64> {
        let mut paragraphs = self.document.paragraphs.clone();
        let index = match after_id {
            Some(id) => paragraphs
                .iter()
                .position(|paragraph| paragraph.id == id)
                .map(|index| index + 1)
                .ok_or_else(|| invalid_input("paragraph ID does not exist"))?,
            None => paragraphs.len(),
        };
        let id = self.document.next_block_id;
        let next_id = id
            .checked_add(1)
            .ok_or_else(|| invalid_input("paragraph ID space exhausted"))?;
        paragraphs.insert(
            index,
            Paragraph {
                id,
                text: text.into(),
                style,
                list_kind: ListKind::None,
                list_level: 0,
                spans: Vec::new(),
            },
        );
        let mut next = self.document.with_paragraphs(paragraphs, next_id)?;
        let new_block = DocumentBlock::Paragraph(id);
        // with_paragraphs ensures every paragraph is represented; reposition
        // the newly created block instead of retaining its temporary append.
        next.block_order.retain(|block| *block != new_block);
        let order_index = match after_id {
            Some(anchor) => next
                .block_order
                .iter()
                .position(|block| *block == DocumentBlock::Paragraph(anchor))
                .map(|i| i + 1)
                .ok_or_else(|| invalid_input("paragraph block position is missing"))?,
            None => next.block_order.len(),
        };
        next.block_order.insert(order_index, new_block);
        validate_document_size(&next)?;
        self.record_change(next)?;
        Ok(id)
    }

    /// Remove a paragraph while preserving at least one editable paragraph.
    pub fn delete_paragraph(&mut self, paragraph_id: u64) -> io::Result<u64> {
        if self.document.paragraphs.len() == 1 {
            return Err(invalid_input("cannot delete the document's only paragraph"));
        }
        let mut paragraphs = self.document.paragraphs.clone();
        let index = paragraphs
            .iter()
            .position(|paragraph| paragraph.id == paragraph_id)
            .ok_or_else(|| invalid_input("paragraph ID does not exist"))?;
        paragraphs.remove(index);
        let mut next = self
            .document
            .with_paragraphs(paragraphs, self.document.next_block_id)?;
        next.block_order
            .retain(|block| *block != DocumentBlock::Paragraph(paragraph_id));
        validate_document_size(&next)?;
        self.record_change(next)
    }

    fn record_change(&mut self, next: Document) -> io::Result<u64> {
        let encoded = encode_document(&next)?;
        let sequence = self.journal.append(&encoded)?;
        self.document = next;
        self.autosave.mark_dirty(Instant::now());
        Ok(sequence)
    }

    /// Save only when the autosave policy says the document is due.
    pub fn autosave_if_due(&mut self, now: Instant) -> io::Result<bool> {
        if !self.autosave.should_save(now) {
            return Ok(false);
        }
        self.save_now()?;
        Ok(true)
    }

    /// Persist and verify a checkpoint, then compact the covered journal records.
    pub fn save_now(&mut self) -> io::Result<()> {
        if !self.autosave.is_dirty() && self.filesystem.exists(&self.checkpoint_path()) {
            return Ok(());
        }
        let sequence = self.write_checkpoint_without_compaction()?;
        self.journal.reset_after_checkpoint(sequence)?;
        self.autosave.mark_saved();
        self.recovered_records = 0;
        self.journal_had_truncated_tail = false;
        Ok(())
    }

    fn write_checkpoint_without_compaction(&mut self) -> io::Result<u64> {
        let generation = self
            .checkpoint_generation
            .checked_add(1)
            .ok_or_else(|| invalid_input("checkpoint generation exhausted"))?;
        let sequence = self.journal.latest_sequence()?;
        let checkpoint = Checkpoint::new(generation, sequence, encode_document(&self.document)?);
        let path = self.checkpoint_path();
        write_checkpoint(&self.filesystem, &path, &checkpoint)?;
        verify_checkpoint(&self.filesystem, &path, generation)?;
        self.checkpoint_generation = generation;
        Ok(sequence)
    }

    fn checkpoint_path(&self) -> PathBuf {
        self.directory.join(CHECKPOINT_FILE)
    }
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "likounsee-document-engine-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn markdown_export_preserves_heading_lists_tables_and_escapes_cells() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        session.set_title("Release notes").unwrap();
        session
            .set_body("Overview\nFirst item\nSecond item")
            .unwrap();
        let first_paragraph_id = session.document().paragraphs()[0].id();
        let second_paragraph_id = session.document().paragraphs()[1].id();
        session
            .set_paragraph_style(first_paragraph_id, ParagraphStyle::Heading1)
            .unwrap();
        session
            .set_paragraph_list(second_paragraph_id, ListKind::Bullet, 0)
            .unwrap();
        session
            .set_inline_mark(first_paragraph_id, 0..8, InlineMark::Bold, true)
            .unwrap();
        let table_id = session.insert_table(2, 2).unwrap();
        session
            .set_table_cell_text(table_id, 0, 0, "A | B")
            .unwrap();
        let markdown = export_markdown(session.document()).unwrap();
        assert!(markdown.contains("# Release notes"));
        assert!(markdown.contains("# **Overview**"));
        assert!(markdown.contains("- First item"));
        assert!(markdown.contains("A \\| B"));
        assert!(markdown.contains("| --- | --- |"));
    }

    #[test]
    fn document_encoding_round_trips_unicode_and_revision() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        session.set_title("Résumé 📄").unwrap();
        session.set_body("Bonjour, 世界!\n").unwrap();
        let encoded = encode_document(session.document()).unwrap();
        assert_eq!(decode_document(&encoded).unwrap(), *session.document());
    }

    #[test]
    fn rejects_invalid_document_data() {
        assert_eq!(
            decode_document(b"bad").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut encoded = encode_document(&Document::new("Title").unwrap()).unwrap();
        encoded[8..12].copy_from_slice(&99_u32.to_le_bytes());
        assert_eq!(
            decode_document(&encoded).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn recovers_latest_journalled_document_after_session_is_dropped() {
        let directory = TestDirectory::new();
        {
            let mut session = DocumentSession::open(&directory.0).unwrap();
            session.set_title("My notes").unwrap();
            session.set_body("first\nsecond").unwrap();
            assert_eq!(session.document().revision(), 2);
        }

        let recovered = DocumentSession::open(&directory.0).unwrap();
        assert_eq!(recovered.document().title(), "My notes");
        assert_eq!(recovered.document().body(), "first\nsecond");
        assert_eq!(recovered.document().revision(), 2);
        assert_eq!(recovered.recovered_records(), 2);
    }

    #[test]
    fn checkpoint_before_journal_reset_does_not_replay_old_edits() {
        let directory = TestDirectory::new();
        {
            let mut session = DocumentSession::open(&directory.0).unwrap();
            session.set_body("persisted once").unwrap();
            session.write_checkpoint_without_compaction().unwrap();
            // Simulate a crash after checkpoint replacement and before journal reset.
        }

        let recovered = DocumentSession::open(&directory.0).unwrap();
        assert_eq!(recovered.document().body(), "persisted once");
        assert_eq!(recovered.document().revision(), 1);
        assert_eq!(recovered.recovered_records(), 0);
    }

    #[test]
    fn edits_after_checkpoint_are_not_discarded_during_recovery() {
        let directory = TestDirectory::new();
        {
            let mut session = DocumentSession::open(&directory.0).unwrap();
            assert_eq!(session.set_body("checkpoint state").unwrap(), 1);
            assert_eq!(session.write_checkpoint_without_compaction().unwrap(), 1);
            assert_eq!(session.set_body("newer journal state").unwrap(), 2);
            // Simulate a crash before compaction. Only sequence 1 is in the checkpoint.
        }

        let recovered = DocumentSession::open(&directory.0).unwrap();
        assert_eq!(recovered.document().body(), "newer journal state");
        assert_eq!(recovered.document().revision(), 2);
        assert_eq!(recovered.recovered_records(), 1);
    }

    #[test]
    fn autosave_compacts_journal_and_keeps_sequence_monotonic() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        assert_eq!(session.set_body("before checkpoint").unwrap(), 1);
        session.save_now().unwrap();
        assert_eq!(session.set_body("after checkpoint").unwrap(), 2);
        assert_eq!(session.document().body(), "after checkpoint");

        let recovered = DocumentSession::open(&directory.0).unwrap();
        assert_eq!(recovered.document().body(), "after checkpoint");
        assert_eq!(recovered.document().revision(), 2);
        assert_eq!(recovered.recovered_records(), 1);
    }

    #[test]
    fn autosave_waits_for_debounce_then_writes_checkpoint() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        session.set_body("autosaved").unwrap();
        assert!(!session.autosave_if_due(Instant::now()).unwrap());
        assert!(
            session
                .autosave_if_due(Instant::now() + Duration::from_secs(3))
                .unwrap()
        );

        let reopened = DocumentSession::open(&directory.0).unwrap();
        assert_eq!(reopened.document().body(), "autosaved");
        assert_eq!(reopened.recovered_records(), 0);
    }

    #[test]
    fn targeted_text_edits_preserve_utf8_and_increment_revisions() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        session.set_body("Aé🧪Z").unwrap();
        session.insert_text(1, " + ").unwrap();
        session.replace_range(4..6, "è").unwrap();
        session.delete_range(1..4).unwrap();

        assert_eq!(session.document().body(), "Aè🧪Z");
        assert_eq!(session.document().revision(), 4);
    }

    #[test]
    fn targeted_text_edits_reject_invalid_utf8_boundaries_without_mutation() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        session.set_body("aéz").unwrap();
        let before = session.document().clone();

        assert!(session.insert_text(2, "x").is_err());
        assert!(session.delete_range(2..3).is_err());
        let reversed_start = 3;
        let reversed_end = 2;
        assert!(
            session
                .replace_range(reversed_start..reversed_end, "x")
                .is_err()
        );
        assert_eq!(session.document(), &before);
    }

    #[test]
    fn targeted_text_edits_are_recovered_from_the_journal() {
        let directory = TestDirectory::new();
        {
            let mut session = DocumentSession::open(&directory.0).unwrap();
            session.set_body("Hello world").unwrap();
            session.replace_range(6..11, "OpenFS").unwrap();
        }

        let recovered = DocumentSession::open(&directory.0).unwrap();
        assert_eq!(recovered.document().body(), "Hello OpenFS");
        assert_eq!(recovered.document().revision(), 2);
    }

    #[test]
    fn structured_paragraphs_keep_stable_ids_and_styles_through_recovery() {
        let directory = TestDirectory::new();
        let (first_id, second_id);
        {
            let mut session = DocumentSession::open(&directory.0).unwrap();
            first_id = session.document().paragraphs()[0].id();
            second_id = session
                .insert_paragraph_after(
                    Some(first_id),
                    "Second paragraph",
                    ParagraphStyle::Heading1,
                )
                .unwrap();
            session
                .insert_paragraph_after(Some(second_id), "A quotation", ParagraphStyle::Quote)
                .unwrap();
            session
                .set_paragraph_style(first_id, ParagraphStyle::Heading2)
                .unwrap();
            session.delete_paragraph(second_id).unwrap();
            assert_eq!(session.document().body(), "\nA quotation");
        }
        let recovered = DocumentSession::open(&directory.0).unwrap();
        assert_eq!(recovered.document().paragraphs()[0].id(), first_id);
        assert_eq!(
            recovered.document().paragraphs()[0].style(),
            ParagraphStyle::Heading2
        );
        assert_eq!(
            recovered.document().paragraphs()[1].style(),
            ParagraphStyle::Quote
        );
        assert_ne!(recovered.document().paragraphs()[1].id(), second_id);
    }

    #[test]
    fn legacy_v1_documents_decode_into_normal_paragraphs() {
        let title = "Old";
        let body = "first\nsecond";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(DOCUMENT_MAGIC);
        bytes.extend_from_slice(&LEGACY_DOCUMENT_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&7_u64.to_le_bytes());
        bytes.extend_from_slice(&(title.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(body.len() as u64).to_le_bytes());
        bytes.extend_from_slice(title.as_bytes());
        bytes.extend_from_slice(body.as_bytes());

        let document = decode_document(&bytes).unwrap();
        assert_eq!(document.title(), title);
        assert_eq!(document.body(), body);
        assert_eq!(document.revision(), 7);
        assert_eq!(document.paragraphs().len(), 2);
        assert_eq!(document.paragraphs()[0].style(), ParagraphStyle::Normal);
        assert_eq!(document.paragraphs()[1].text(), "second");
    }

    #[test]
    fn structured_document_format_round_trips_and_rejects_invalid_style() {
        let mut document = Document::new("Structured").unwrap();
        document.paragraphs[0].text = "Heading".to_owned();
        document.paragraphs[0].style = ParagraphStyle::Heading1;
        document.body = "Heading".to_owned();
        let encoded = encode_document(&document).unwrap();
        assert_eq!(decode_document(&encoded).unwrap(), document);

        let mut corrupted = encoded;
        let style_offset =
            DOCUMENT_HEADER_SIZE + document.title.len() + document.body.len() + 12 + 8;
        corrupted[style_offset] = 255;
        assert_eq!(
            decode_document(&corrupted).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn paragraph_lists_round_trip_and_survive_recovery() {
        let directory = TestDirectory::new();
        let first_id;
        {
            let mut session = DocumentSession::open(&directory.0).unwrap();
            first_id = session.document().paragraphs()[0].id();
            session
                .set_body("First item\nNested item\nNumbered item")
                .unwrap();
            let second_id = session.document().paragraphs()[1].id();
            let third_id = session.document().paragraphs()[2].id();
            session
                .set_paragraph_list(first_id, ListKind::Bullet, 0)
                .unwrap();
            session
                .set_paragraph_list(second_id, ListKind::Bullet, 2)
                .unwrap();
            session
                .set_paragraph_list(third_id, ListKind::Numbered, 0)
                .unwrap();
            let encoded = encode_document(session.document()).unwrap();
            assert_eq!(decode_document(&encoded).unwrap(), *session.document());
        }

        let recovered = DocumentSession::open(&directory.0).unwrap();
        let paragraphs = recovered.document().paragraphs();
        assert_eq!(paragraphs[0].id(), first_id);
        assert_eq!(paragraphs[0].list_kind(), ListKind::Bullet);
        assert_eq!(paragraphs[0].list_level(), 0);
        assert_eq!(paragraphs[1].list_kind(), ListKind::Bullet);
        assert_eq!(paragraphs[1].list_level(), 2);
        assert_eq!(paragraphs[2].list_kind(), ListKind::Numbered);
    }

    #[test]
    fn paragraph_list_metadata_rejects_unknown_kinds_and_invalid_levels() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        let paragraph_id = session.document().paragraphs()[0].id();
        let before = session.document().clone();
        assert!(
            session
                .set_paragraph_list(paragraph_id, ListKind::None, 1)
                .is_err()
        );
        assert!(
            session
                .set_paragraph_list(paragraph_id, ListKind::Bullet, 9)
                .is_err()
        );
        assert_eq!(session.document(), &before);

        session
            .set_paragraph_list(paragraph_id, ListKind::Bullet, 1)
            .unwrap();
        let mut encoded = encode_document(session.document()).unwrap();
        let list_kind_offset = DOCUMENT_HEADER_SIZE
            + session.document().title.len()
            + session.document().body.len()
            + 12
            + 8
            + 1;
        encoded[list_kind_offset] = 255;
        assert_eq!(
            decode_document(&encoded).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn version_two_documents_decode_without_list_metadata() {
        let title = "Previous";
        let body = "legacy list model";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(DOCUMENT_MAGIC);
        bytes.extend_from_slice(&LEGACY_STRUCTURED_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&4_u64.to_le_bytes());
        bytes.extend_from_slice(&(title.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(body.len() as u64).to_le_bytes());
        bytes.extend_from_slice(title.as_bytes());
        bytes.extend_from_slice(body.as_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u64.to_le_bytes());
        bytes.extend_from_slice(&1_u64.to_le_bytes());
        bytes.push(ParagraphStyle::Normal as u8);
        bytes.extend_from_slice(&(body.len() as u64).to_le_bytes());
        bytes.extend_from_slice(body.as_bytes());

        let document = decode_document(&bytes).unwrap();
        assert_eq!(document.body(), body);
        assert_eq!(document.paragraphs()[0].list_kind(), ListKind::None);
        assert_eq!(document.paragraphs()[0].list_level(), 0);
    }

    #[test]
    fn inline_marks_round_trip_and_survive_recovery() {
        let directory = TestDirectory::new();
        let paragraph_id;
        {
            let mut session = DocumentSession::open(&directory.0).unwrap();
            session.set_body("Bold and italic text").unwrap();
            paragraph_id = session.document().paragraphs()[0].id();
            session
                .set_inline_mark(paragraph_id, 0..4, InlineMark::Bold, true)
                .unwrap();
            session
                .set_inline_mark(paragraph_id, 9..16, InlineMark::Italic, true)
                .unwrap();
            session
                .set_inline_mark(paragraph_id, 0..16, InlineMark::Underline, true)
                .unwrap();
            session
                .set_inline_mark(paragraph_id, 1..3, InlineMark::Underline, false)
                .unwrap();
            let spans = session.document().paragraphs()[0].spans();
            assert!(spans.iter().any(|span| span.mark() == InlineMark::Bold
                && span.start() == 0
                && span.end() == 4));
            assert!(spans.iter().any(|span| span.mark() == InlineMark::Italic
                && span.start() == 9
                && span.end() == 16));
            assert!(spans.iter().any(|span| span.mark() == InlineMark::Underline
                && span.start() == 0
                && span.end() == 1));
            assert!(spans.iter().any(|span| span.mark() == InlineMark::Underline
                && span.start() == 3
                && span.end() == 16));
            let encoded = encode_document(session.document()).unwrap();
            assert_eq!(decode_document(&encoded).unwrap(), *session.document());
        }
        let recovered = DocumentSession::open(&directory.0).unwrap();
        assert_eq!(recovered.document().paragraphs()[0].spans().len(), 4);
        assert_eq!(recovered.document().paragraphs()[0].id(), paragraph_id);
    }

    #[test]
    fn inline_marks_track_single_paragraph_text_edits() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        session.set_body("Bold text").unwrap();
        let id = session.document().paragraphs()[0].id();
        session
            .set_inline_mark(id, 0..4, InlineMark::Bold, true)
            .unwrap();
        session.insert_text(2, "XX").unwrap();
        let span = &session.document().paragraphs()[0].spans()[0];
        assert_eq!(
            (span.start(), span.end(), span.mark()),
            (0, 6, InlineMark::Bold)
        );
        session.replace_range(1..4, "Y").unwrap();
        let span = &session.document().paragraphs()[0].spans()[0];
        assert_eq!(
            (span.start(), span.end(), span.mark()),
            (0, 4, InlineMark::Bold)
        );
        let recovered = DocumentSession::open(&directory.0).unwrap();
        assert_eq!(recovered.document().paragraphs()[0].spans()[0].end(), 4);
    }

    #[test]
    fn inline_marks_reject_invalid_utf8_ranges_without_mutation() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        session.set_body("Aé🧪Z").unwrap();
        let id = session.document().paragraphs()[0].id();
        let before = session.document().clone();
        assert!(
            session
                .set_inline_mark(id, 2..4, InlineMark::Bold, true)
                .is_err()
        );
        assert!(
            session
                .set_inline_mark(id, 1..1, InlineMark::Italic, true)
                .is_err()
        );
        assert_eq!(session.document(), &before);
    }

    #[test]
    fn version_three_documents_decode_without_inline_spans() {
        let title = "Previous";
        let body = "legacy list model";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(DOCUMENT_MAGIC);
        bytes.extend_from_slice(&LEGACY_LIST_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&4_u64.to_le_bytes());
        bytes.extend_from_slice(&(title.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(body.len() as u64).to_le_bytes());
        bytes.extend_from_slice(title.as_bytes());
        bytes.extend_from_slice(body.as_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u64.to_le_bytes());
        bytes.extend_from_slice(&1_u64.to_le_bytes());
        bytes.push(ParagraphStyle::Normal as u8);
        bytes.push(ListKind::Bullet as u8);
        bytes.push(0);
        bytes.extend_from_slice(&(body.len() as u64).to_le_bytes());
        bytes.extend_from_slice(body.as_bytes());

        let document = decode_document(&bytes).unwrap();
        assert_eq!(document.body(), body);
        assert_eq!(document.paragraphs()[0].list_kind(), ListKind::Bullet);
        assert!(document.paragraphs()[0].spans().is_empty());
    }

    #[test]
    fn tables_round_trip_and_survive_journal_recovery() {
        let directory = TestDirectory::new();
        let table_id;
        let cell_id;
        {
            let mut session = DocumentSession::open(&directory.0).unwrap();
            table_id = session.insert_table(2, 3).unwrap();
            session.set_table_cell_text(table_id, 0, 0, "Nom").unwrap();
            session
                .set_table_cell_text(table_id, 0, 1, "Valeur")
                .unwrap();
            session
                .set_table_cell_text(table_id, 1, 0, "Élément 🧪")
                .unwrap();
            let table = &session.document().tables()[0];
            cell_id = table.rows()[0].cells()[0].id();
            assert_eq!(table.columns(), 3);
            assert_eq!(table.rows().len(), 2);
            assert_eq!(table.rows()[1].cells()[0].text(), "Élément 🧪");
            let encoded = encode_document(session.document()).unwrap();
            assert_eq!(decode_document(&encoded).unwrap(), *session.document());
        }

        let recovered = DocumentSession::open(&directory.0).unwrap();
        let table = &recovered.document().tables()[0];
        assert_eq!(table.id(), table_id);
        assert_eq!(table.rows()[0].cells()[0].id(), cell_id);
        assert_eq!(table.rows()[0].cells()[0].text(), "Nom");
        assert_eq!(table.rows()[0].cells()[1].text(), "Valeur");
        assert_eq!(table.rows()[1].cells()[0].text(), "Élément 🧪");
    }

    #[test]
    fn invalid_table_dimensions_and_cell_coordinates_do_not_mutate_document() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        let before = session.document().clone();
        assert!(session.insert_table(0, 2).is_err());
        assert!(session.insert_table(1025, 1).is_err());
        assert_eq!(session.document(), &before);

        let table_id = session.insert_table(1, 1).unwrap();
        let before_invalid_cell = session.document().clone();
        assert!(session.set_table_cell_text(table_id, 1, 0, "bad").is_err());
        assert!(session.set_table_cell_text(table_id, 0, 1, "bad").is_err());
        assert_eq!(session.document(), &before_invalid_cell);
    }

    #[test]
    fn table_rows_and_columns_can_be_inserted_deleted_and_recovered() {
        let directory = TestDirectory::new();
        let table_id;
        let original_row_id;
        let original_cell_id;
        let inserted_row_id;
        {
            let mut session = DocumentSession::open(&directory.0).unwrap();
            table_id = session.insert_table(2, 2).unwrap();
            original_row_id = session.document().tables()[0].rows()[0].id();
            original_cell_id = session.document().tables()[0].rows()[0].cells()[0].id();
            inserted_row_id = session.insert_table_row(table_id, 1).unwrap();
            let previous_next_id = session.document().next_block_id;
            let inserted_column_cell = session.insert_table_column(table_id, 1).unwrap();
            assert_eq!(inserted_column_cell, previous_next_id);
            assert_eq!(session.document().tables()[0].rows().len(), 3);
            assert_eq!(session.document().tables()[0].columns(), 3);
            assert_eq!(
                session.document().tables()[0].rows()[0].id(),
                original_row_id
            );
            assert_eq!(
                session.document().tables()[0].rows()[0].cells()[0].id(),
                original_cell_id
            );
            assert_eq!(
                session.document().tables()[0].rows()[1].id(),
                inserted_row_id
            );
            assert!(
                session.document().tables()[0]
                    .rows()
                    .iter()
                    .all(|row| row.cells()[1].id() >= inserted_column_cell)
            );
            session.delete_table_row(table_id, 1).unwrap();
            session.delete_table_column(table_id, 1).unwrap();
            assert_eq!(session.document().tables()[0].rows().len(), 2);
            assert_eq!(session.document().tables()[0].columns(), 2);
            let encoded = encode_document(session.document()).unwrap();
            assert_eq!(decode_document(&encoded).unwrap(), *session.document());
        }
        let recovered = DocumentSession::open(&directory.0).unwrap();
        assert_eq!(recovered.document().tables()[0].rows().len(), 2);
        assert_eq!(recovered.document().tables()[0].columns(), 2);
        assert_eq!(
            recovered.document().tables()[0].rows()[0].id(),
            original_row_id
        );
    }

    #[test]
    fn table_formatting_round_trips_and_survives_recovery() {
        let directory = TestDirectory::new();
        let table_id;
        {
            let mut session = DocumentSession::open(&directory.0).unwrap();
            table_id = session.insert_table(2, 2).unwrap();
            session.set_table_column_width(table_id, 0, 3600).unwrap();
            session.set_table_row_height(table_id, 1, 720).unwrap();
            session
                .set_table_cell_alignment(table_id, 0, 1, CellAlignment::Center)
                .unwrap();
            session
                .set_table_cell_background(table_id, 0, 1, Some(0x12_34_56))
                .unwrap();
            session
                .set_table_cell_borders(table_id, 0, 1, 0b0101)
                .unwrap();

            let table = &session.document().tables()[0];
            assert_eq!(table.column_widths_twips(), &[3600, 2400]);
            assert_eq!(table.rows()[1].height_twips(), 720);
            let cell = &table.rows()[0].cells()[1];
            assert_eq!(cell.alignment(), CellAlignment::Center);
            assert_eq!(cell.background(), Some(0x12_34_56));
            assert_eq!(cell.borders(), 0b0101);

            let encoded = encode_document(session.document()).unwrap();
            assert_eq!(decode_document(&encoded).unwrap(), *session.document());
        }
        let recovered = DocumentSession::open(&directory.0).unwrap();
        let table = &recovered.document().tables()[0];
        assert_eq!(table.id(), table_id);
        assert_eq!(table.column_widths_twips(), &[3600, 2400]);
        assert_eq!(table.rows()[1].height_twips(), 720);
        assert_eq!(
            table.rows()[0].cells()[1].alignment(),
            CellAlignment::Center
        );
        assert_eq!(table.rows()[0].cells()[1].background(), Some(0x12_34_56));
        assert_eq!(table.rows()[0].cells()[1].borders(), 0b0101);
    }

    #[test]
    fn version_six_tables_migrate_with_default_formatting() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        session.insert_table(1, 2).unwrap();
        let mut bytes = encode_document(session.document()).unwrap();
        // v7 formatting trailer: count + table ID + widths + row + cell records.
        bytes.truncate(bytes.len() - 4 - 8 - 2 * 4 - 12 - 2 * 14);
        bytes[8..12].copy_from_slice(&6_u32.to_le_bytes());
        let migrated = decode_document(&bytes).unwrap();
        let table = &migrated.tables()[0];
        assert_eq!(table.column_widths_twips(), &[2400, 2400]);
        assert_eq!(table.rows()[0].height_twips(), 0);
        assert_eq!(table.rows()[0].cells()[0].alignment(), CellAlignment::Left);
        assert_eq!(table.rows()[0].cells()[0].background(), None);
        assert_eq!(table.rows()[0].cells()[0].borders(), 0x0f);
    }

    #[test]
    fn truncated_table_formatting_trailer_is_rejected() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        session.insert_table(1, 1).unwrap();
        let mut bytes = encode_document(session.document()).unwrap();
        bytes.pop();
        assert!(decode_document(&bytes).is_err());
    }

    #[test]
    fn invalid_table_formatting_is_rejected_without_mutation() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        let table_id = session.insert_table(1, 1).unwrap();
        let before = session.document().clone();
        assert!(session.set_table_column_width(table_id, 0, 99).is_err());
        assert!(session.set_table_column_width(table_id, 1, 2400).is_err());
        assert!(session.set_table_row_height(table_id, 0, 99).is_err());
        assert!(
            session
                .set_table_cell_background(table_id, 0, 0, Some(0x01_00_00_00))
                .is_err()
        );
        assert!(
            session
                .set_table_cell_borders(table_id, 0, 0, 0x10)
                .is_err()
        );
        assert!(
            session
                .set_table_cell_alignment(table_id, 1, 0, CellAlignment::Right)
                .is_err()
        );
        assert_eq!(session.document(), &before);
    }

    #[test]
    fn table_row_and_column_deletion_preserve_minimum_dimensions() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        let table_id = session.insert_table(1, 1).unwrap();
        let before = session.document().clone();
        assert!(session.delete_table_row(table_id, 0).is_err());
        assert!(session.delete_table_column(table_id, 0).is_err());
        assert!(session.delete_table_row(table_id, 1).is_err());
        assert!(session.delete_table_column(table_id, 1).is_err());
        assert!(session.insert_table_row(table_id, 2).is_err());
        assert!(session.insert_table_column(table_id, 2).is_err());
        assert_eq!(session.document(), &before);
    }

    #[test]
    fn version_four_documents_decode_without_tables() {
        let mut document = Document::new("Version four").unwrap();
        document.body = "legacy".to_owned();
        document.paragraphs[0].text = "legacy".to_owned();
        let mut bytes = encode_document(&document).unwrap();
        // Remove the v5 table trailer, v6 block-order trailer and v7 formatting count.
        bytes.truncate(bytes.len() - 21);
        bytes[8..12].copy_from_slice(&4_u32.to_le_bytes());
        let decoded = decode_document(&bytes).unwrap();
        assert_eq!(decoded.body(), "legacy");
        assert!(decoded.tables().is_empty());
        assert_eq!(
            decoded.blocks(),
            &[DocumentBlock::Paragraph(decoded.paragraphs()[0].id())]
        );
    }

    #[test]
    fn unified_block_order_interleaves_paragraphs_and_tables_and_survives_recovery() {
        let directory = TestDirectory::new();
        let first_id;
        let table_id;
        let inserted_id;
        {
            let mut session = DocumentSession::open(&directory.0).unwrap();
            first_id = session.document().paragraphs()[0].id();
            table_id = session.insert_table(1, 2).unwrap();
            inserted_id = session
                .insert_paragraph_after(
                    Some(first_id),
                    "Paragraph between blocks",
                    ParagraphStyle::Normal,
                )
                .unwrap();
            assert_eq!(
                session.document().blocks(),
                &[
                    DocumentBlock::Paragraph(first_id),
                    DocumentBlock::Paragraph(inserted_id),
                    DocumentBlock::Table(table_id),
                ]
            );
            let encoded = encode_document(session.document()).unwrap();
            assert_eq!(decode_document(&encoded).unwrap(), *session.document());
        }
        let recovered = DocumentSession::open(&directory.0).unwrap();
        assert_eq!(
            recovered.document().blocks(),
            &[
                DocumentBlock::Paragraph(first_id),
                DocumentBlock::Paragraph(inserted_id),
                DocumentBlock::Table(table_id),
            ]
        );
    }

    #[test]
    fn find_text_returns_paragraph_and_table_cell_ranges() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        session.insert_text(0, "one needle needle").unwrap();
        let table_id = session.insert_table(1, 1).unwrap();
        session
            .set_table_cell_text(table_id, 0, 0, "needle in cell")
            .unwrap();
        let document = session.document().clone();
        let matches = find_text(&document, "needle").unwrap();
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].cell_id, None);
        assert_eq!(matches[2].cell_id.is_some(), true);
        assert!(find_text(&document, "").is_err());
    }

    #[test]
    fn character_count_counts_unicode_scalars_and_whitespace() {
        let document = Document::from_plain_text("Count", "A é\n猫").unwrap();
        assert_eq!(character_count(&document).unwrap(), 4);
    }

    #[test]
    fn word_count_includes_paragraphs_and_table_cells() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        session.insert_text(0, "Hello world").unwrap();
        let table_id = session.insert_table(1, 2).unwrap();
        session
            .set_table_cell_text(table_id, 0, 0, "three")
            .unwrap();
        session
            .set_table_cell_text(table_id, 0, 1, "four five")
            .unwrap();
        assert_eq!(word_count(session.document()).unwrap(), 5);
    }

    #[test]
    fn plain_text_import_and_export_preserve_lines_and_table_cells() {
        let document = Document::from_plain_text("Notes", "First line\r\nSecond line\n").unwrap();
        assert_eq!(document.paragraphs().len(), 3);
        assert_eq!(document.paragraphs()[0].text(), "First line");
        assert_eq!(document.paragraphs()[1].text(), "Second line");
        assert_eq!(
            export_plain_text(&document).unwrap(),
            "First line\nSecond line\n"
        );
    }

    #[test]
    fn html_export_preserves_semantics_tables_and_escapes_untrusted_text() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        session.insert_text(0, "Safe <text> & \"quotes\"").unwrap();
        let paragraph_id = session.document().paragraphs()[0].id();
        session
            .set_paragraph_style(paragraph_id, ParagraphStyle::Heading1)
            .unwrap();
        session
            .set_inline_mark(paragraph_id, 0..4, InlineMark::Bold, true)
            .unwrap();
        let table_id = session.insert_table(1, 2).unwrap();
        session
            .set_table_cell_text(table_id, 0, 0, "A & B")
            .unwrap();
        let html = export_html(session.document()).unwrap();
        assert!(html.contains("<h2><strong>Safe</strong>"));
        assert!(html.contains("&lt;text&gt; &amp; &quot;quotes&quot;"));
        assert!(html.contains("<td>A &amp; B</td>"));
        assert!(html.contains("<!doctype html>"));
    }

    #[test]
    fn plain_text_export_preserves_mixed_block_order_and_table_cells() {
        let directory = TestDirectory::new();
        let mut session = DocumentSession::open(&directory.0).unwrap();
        session.insert_text(0, "First paragraph").unwrap();
        let table_id = session.insert_table(2, 2).unwrap();
        session.set_table_cell_text(table_id, 0, 0, "A").unwrap();
        session.set_table_cell_text(table_id, 0, 1, "B").unwrap();
        session.set_table_cell_text(table_id, 1, 0, "C").unwrap();
        session.set_table_cell_text(table_id, 1, 1, "D").unwrap();
        let text = export_text(session.document()).unwrap();
        assert_eq!(text, "First paragraph\nA\tB\nC\tD\n");
    }

    #[test]
    fn failed_edit_does_not_change_in_memory_document() {
        let mut document = Document::new("Title").unwrap();
        document.revision = u64::MAX;
        assert!(document.with_body("next".to_owned()).is_err());
        assert_eq!(document.body(), "");
        assert_eq!(document.revision(), u64::MAX);
    }
}
