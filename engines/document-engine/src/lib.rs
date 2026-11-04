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
const DOCUMENT_FORMAT_VERSION: u32 = 1;
const DOCUMENT_HEADER_SIZE: usize = 32;
const MAX_DOCUMENT_SIZE: usize = 64 * 1024 * 1024 - DOCUMENT_HEADER_SIZE;
const MAX_TITLE_SIZE: usize = 1024 * 1024;
const CHECKPOINT_FILE: &str = "document.checkpoint";
const JOURNAL_FILE: &str = "document.journal";

/// A minimal editable document. Revision increases once for each successful edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    title: String,
    body: String,
    revision: u64,
}

impl Document {
    pub fn new(title: impl Into<String>) -> io::Result<Self> {
        let title = title.into();
        if title.len() > MAX_TITLE_SIZE {
            return Err(invalid_input("document title exceeds 1 MiB"));
        }
        Ok(Self {
            title,
            body: String::new(),
            revision: 0,
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

    fn with_title(&self, title: String) -> io::Result<Self> {
        if title.len() > MAX_TITLE_SIZE {
            return Err(invalid_input("document title exceeds 1 MiB"));
        }
        let mut next = self.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid_input("document revision exhausted"))?;
        next.title = title;
        validate_document_size(&next)?;
        Ok(next)
    }

    fn with_body(&self, body: String) -> io::Result<Self> {
        let mut next = self.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid_input("document revision exhausted"))?;
        next.body = body;
        validate_document_size(&next)?;
        Ok(next)
    }
}

/// Encode a document using the engine's initial versioned binary format.
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
    Ok(bytes)
}

/// Decode a complete document, rejecting unknown versions and malformed UTF-8.
pub fn decode_document(bytes: &[u8]) -> io::Result<Document> {
    if bytes.len() < DOCUMENT_HEADER_SIZE || &bytes[..8] != DOCUMENT_MAGIC {
        return Err(invalid_data("invalid document header"));
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if version != DOCUMENT_FORMAT_VERSION {
        return Err(invalid_data("unsupported document format version"));
    }
    let revision = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
    let title_len = u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;
    let body_len = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    let body_len = usize::try_from(body_len)
        .map_err(|_| invalid_data("document body length is not representable"))?;
    let expected_len = DOCUMENT_HEADER_SIZE
        .checked_add(title_len)
        .and_then(|len| len.checked_add(body_len))
        .ok_or_else(|| invalid_data("document size overflow"))?;

    if expected_len != bytes.len() || expected_len > MAX_DOCUMENT_SIZE || title_len > MAX_TITLE_SIZE
    {
        return Err(invalid_data("document length is invalid"));
    }

    let title_end = DOCUMENT_HEADER_SIZE + title_len;
    let title = std::str::from_utf8(&bytes[DOCUMENT_HEADER_SIZE..title_end])
        .map_err(|_| invalid_data("document title is not valid UTF-8"))?;
    let body = std::str::from_utf8(&bytes[title_end..])
        .map_err(|_| invalid_data("document body is not valid UTF-8"))?;

    Ok(Document {
        title: title.to_owned(),
        body: body.to_owned(),
        revision,
    })
}

fn validate_document_size(document: &Document) -> io::Result<()> {
    let size = DOCUMENT_HEADER_SIZE
        .checked_add(document.title.len())
        .and_then(|len| len.checked_add(document.body.len()))
        .ok_or_else(|| invalid_input("document size overflow"))?;
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
        let mut body = self.document.body.clone();
        if offset > body.len() || !body.is_char_boundary(offset) {
            return Err(invalid_input(
                "text insertion offset is not a UTF-8 character boundary",
            ));
        }
        body.insert_str(offset, text);
        let next = self.document.with_body(body)?;
        self.record_change(next)
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
        body.replace_range(range, replacement);
        let next = self.document.with_body(body)?;
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
        assert!(session.replace_range(3..2, "x").is_err());
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
    fn failed_edit_does_not_change_in_memory_document() {
        let mut document = Document::new("Title").unwrap();
        document.revision = u64::MAX;
        assert!(document.with_body("next".to_owned()).is_err());
        assert_eq!(document.body(), "");
        assert_eq!(document.revision(), u64::MAX);
    }
}
