//! Append-only recovery journal for autosave snapshots or edit batches.
//!
//! Each record has a monotonically increasing sequence, bounded payload length
//! and CRC-32. Replay streams records from disk and stops at the first invalid tail.

use crc32fast::Hasher;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const FILE_MAGIC: &[u8; 8] = b"LOUSJNL2";
const FILE_HEADER_SIZE: u64 = 16;
const RECORD_HEADER_SIZE: usize = 16;
const MAX_RECORD_SIZE: usize = 64 * 1024 * 1024;
const MAX_JOURNAL_SIZE: u64 = 256 * 1024 * 1024;
const SCAN_BUFFER_SIZE: usize = 64 * 1024;

/// One valid journal entry recovered from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    pub sequence: u64,
    pub payload: Vec<u8>,
}

/// Result of scanning a journal. Valid records before a damaged tail are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalReplay {
    /// Sequence covered by the checkpoint preceding this journal segment.
    pub base_sequence: u64,
    pub records: Vec<JournalRecord>,
    pub had_truncated_tail: bool,
    pub valid_bytes: u64,
}

struct ScanResult {
    replay: JournalReplay,
    last_sequence: u64,
}

/// A single-process append-only journal. Keep one instance per journal path.
///
/// The journal syncs each appended record before returning. It does not replace
/// periodic checkpoints: callers should compact old records after a verified
/// checkpoint has been safely written.
pub struct RecoveryJournal {
    path: PathBuf,
    gate: Mutex<()>,
}

impl RecoveryJournal {
    /// Open or initialize a journal. Parent directories must already exist.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        if file.metadata()?.len() == 0 {
            file.write_all(FILE_MAGIC)?;
            file.write_all(&0_u64.to_le_bytes())?;
            file.sync_all()?;
            sync_parent_directory(&path)?;
        } else {
            let mut header = [0_u8; FILE_HEADER_SIZE as usize];
            file.read_exact(&mut header)?;
            if &header[..8] != FILE_MAGIC {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid recovery journal header or unsupported legacy format",
                ));
            }
        }

        Ok(Self {
            path,
            gate: Mutex::new(()),
        })
    }

    /// Append one record and synchronously flush it to the storage device.
    pub fn append(&self, payload: &[u8]) -> io::Result<u64> {
        if payload.len() > MAX_RECORD_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "recovery record exceeds 64 MiB",
            ));
        }

        let _guard = self
            .gate
            .lock()
            .map_err(|_| io::Error::other("recovery journal lock was poisoned"))?;
        let mut file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        let scan = scan(&mut file, false)?;

        let record_size = (RECORD_HEADER_SIZE as u64)
            .checked_add(payload.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "record size overflow"))?;
        let new_size = scan
            .replay
            .valid_bytes
            .checked_add(record_size)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "journal size overflow"))?;
        if new_size > MAX_JOURNAL_SIZE {
            return Err(io::Error::other(
                "recovery journal reached 256 MiB; create a durable checkpoint first",
            ));
        }

        file.set_len(scan.replay.valid_bytes)?;
        file.seek(SeekFrom::Start(scan.replay.valid_bytes))?;

        let sequence = scan.last_sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "recovery sequence exhausted")
        })?;
        let length = payload.len() as u32;
        let checksum = checksum(payload);

        file.write_all(&sequence.to_le_bytes())?;
        file.write_all(&length.to_le_bytes())?;
        file.write_all(&checksum.to_le_bytes())?;
        file.write_all(payload)?;
        file.sync_data()?;

        Ok(sequence)
    }

    /// Replay all complete, checksum-valid records.
    pub fn replay(&self) -> io::Result<JournalReplay> {
        let _guard = self
            .gate
            .lock()
            .map_err(|_| io::Error::other("recovery journal lock was poisoned"))?;
        let mut file = File::open(&self.path)?;
        Ok(scan(&mut file, true)?.replay)
    }

    /// Replay only records not already included in a verified checkpoint.
    pub fn replay_after(&self, checkpoint_sequence: u64) -> io::Result<JournalReplay> {
        let _guard = self.gate.lock().map_err(|_| io::Error::other("recovery journal lock was poisoned"))?;
        let mut file = File::open(&self.path)?;
        let scan = scan(&mut file, true)?;
        if checkpoint_sequence < scan.replay.base_sequence || checkpoint_sequence > scan.last_sequence {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "checkpoint journal sequence is outside this journal segment"));
        }
        let mut replay = scan.replay;
        replay.records.retain(|record| record.sequence > checkpoint_sequence);
        Ok(replay)
    }

    /// Remove records covered by a durable checkpoint while preserving sequence continuity.
    /// The checkpoint must cover exactly the latest valid journal sequence.
    pub fn reset_after_checkpoint(&self, covered_sequence: u64) -> io::Result<()> {
        let _guard = self
            .gate
            .lock()
            .map_err(|_| io::Error::other("recovery journal lock was poisoned"))?;
        let mut journal_file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        let scan = scan(&mut journal_file, false)?;
        if covered_sequence != scan.last_sequence {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "checkpoint must cover the latest valid journal sequence"));
        }
        journal_file.set_len(0)?;
        journal_file.seek(SeekFrom::Start(0))?;
        journal_file.write_all(FILE_MAGIC)?;
        journal_file.write_all(&covered_sequence.to_le_bytes())?;
        journal_file.sync_all()?;
        sync_parent_directory(&self.path)
    }

    /// Return the latest valid sequence, or the base sequence for an empty journal.
    pub fn latest_sequence(&self) -> io::Result<u64> {
        let _guard = self.gate.lock().map_err(|_| io::Error::other("recovery journal lock was poisoned"))?;
        let mut file = File::open(&self.path)?;
        Ok(scan(&mut file, false)?.last_sequence)
    }

    /// Return the path used by this journal.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Scan incrementally. Collecting payloads is optional: append only needs the
/// last sequence and valid end offset, not copies of every previous payload.
fn scan(file: &mut File, collect_records: bool) -> io::Result<ScanResult> {
    file.seek(SeekFrom::Start(0))?;
    let file_len = file.metadata()?.len();

    if file_len < FILE_HEADER_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid recovery journal header",
        ));
    }

    let mut header = [0_u8; FILE_HEADER_SIZE as usize];
    file.read_exact(&mut header)?;
    if &header[..8] != FILE_MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid recovery journal header"));
    }

    let base_sequence = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let scan_limit = file_len.min(MAX_JOURNAL_SIZE);
    let mut offset = FILE_HEADER_SIZE;
    let mut records = Vec::new();
    let mut last_sequence = base_sequence;
    let mut had_truncated_tail = file_len > MAX_JOURNAL_SIZE;

    while offset < scan_limit {
        let remaining = scan_limit - offset;
        if remaining < RECORD_HEADER_SIZE as u64 {
            had_truncated_tail = true;
            break;
        }

        let mut header = [0_u8; RECORD_HEADER_SIZE];
        if let Err(error) = file.read_exact(&mut header) {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                had_truncated_tail = true;
                break;
            }
            return Err(error);
        }

        let sequence = u64::from_le_bytes(header[0..8].try_into().unwrap());
        let length = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        let expected_checksum = u32::from_le_bytes(header[12..16].try_into().unwrap());

        if length > MAX_RECORD_SIZE
            || sequence != last_sequence.checked_add(1).unwrap_or(0)
            || (length as u64) > scan_limit.saturating_sub(offset + RECORD_HEADER_SIZE as u64)
        {
            had_truncated_tail = true;
            break;
        }

        let mut hasher = Hasher::new();
        let mut payload = if collect_records {
            Some(Vec::with_capacity(length))
        } else {
            None
        };
        let mut remaining_payload = length;
        let mut payload_incomplete = false;
        let mut buffer = [0_u8; SCAN_BUFFER_SIZE];

        while remaining_payload > 0 {
            let chunk_len = remaining_payload.min(buffer.len());
            if let Err(error) = file.read_exact(&mut buffer[..chunk_len]) {
                if error.kind() == io::ErrorKind::UnexpectedEof {
                    had_truncated_tail = true;
                    payload_incomplete = true;
                    break;
                }
                return Err(error);
            }
            hasher.update(&buffer[..chunk_len]);
            if let Some(bytes) = payload.as_mut() {
                bytes.extend_from_slice(&buffer[..chunk_len]);
            }
            remaining_payload -= chunk_len;
        }

        if payload_incomplete || hasher.finalize() != expected_checksum {
            had_truncated_tail = true;
            break;
        }

        offset += RECORD_HEADER_SIZE as u64 + length as u64;
        last_sequence = sequence;
        if let Some(payload) = payload {
            records.push(JournalRecord { sequence, payload });
        }
    }

    Ok(ScanResult {
        replay: JournalReplay {
            base_sequence,
            records,
            had_truncated_tail,
            valid_bytes: offset,
        },
        last_sequence,
    })
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn checksum(payload: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(payload);
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestJournal(PathBuf);

    impl TestJournal {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir()
                .join(format!("likounsee-journal-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir.join("recovery.journal"))
        }
    }

    impl Drop for TestJournal {
        fn drop(&mut self) {
            if let Some(parent) = self.0.parent() {
                let _ = fs::remove_dir_all(parent);
            }
        }
    }

    #[test]
    fn appends_and_replays_sequenced_records() {
        let path = TestJournal::new();
        let journal = RecoveryJournal::open(&path.0).unwrap();

        assert_eq!(journal.append(b"snapshot one").unwrap(), 1);
        assert_eq!(journal.append(b"snapshot two").unwrap(), 2);

        let replay = journal.replay().unwrap();
        assert!(!replay.had_truncated_tail);
        assert_eq!(
            replay.records,
            vec![
                JournalRecord {
                    sequence: 1,
                    payload: b"snapshot one".to_vec()
                },
                JournalRecord {
                    sequence: 2,
                    payload: b"snapshot two".to_vec()
                },
            ]
        );
    }

    #[test]
    fn replay_keeps_valid_records_before_a_torn_tail() {
        let path = TestJournal::new();
        let journal = RecoveryJournal::open(&path.0).unwrap();
        journal.append(b"valid").unwrap();
        drop(journal);

        OpenOptions::new()
            .append(true)
            .open(&path.0)
            .unwrap()
            .write_all(&[2, 0, 0, 0])
            .unwrap();

        let journal = RecoveryJournal::open(&path.0).unwrap();
        let replay = journal.replay().unwrap();
        assert!(replay.had_truncated_tail);
        assert_eq!(replay.records.len(), 1);
        assert_eq!(replay.records[0].payload, b"valid");
    }

    #[test]
    fn next_append_discards_only_the_invalid_tail() {
        let path = TestJournal::new();
        let journal = RecoveryJournal::open(&path.0).unwrap();
        journal.append(b"first").unwrap();
        drop(journal);

        OpenOptions::new()
            .append(true)
            .open(&path.0)
            .unwrap()
            .write_all(&[9, 9, 9])
            .unwrap();

        let journal = RecoveryJournal::open(&path.0).unwrap();
        assert_eq!(journal.append(b"second").unwrap(), 2);

        let replay = journal.replay().unwrap();
        assert!(!replay.had_truncated_tail);
        assert_eq!(replay.records.len(), 2);
        assert_eq!(replay.records[1].payload, b"second");
    }

    #[test]
    fn checksum_failure_preserves_earlier_records() {
        let path = TestJournal::new();
        let journal = RecoveryJournal::open(&path.0).unwrap();
        journal.append(b"good record").unwrap();
        journal.append(b"damaged record").unwrap();
        drop(journal);

        let mut bytes = fs::read(&path.0).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(&path.0, bytes).unwrap();

        let journal = RecoveryJournal::open(&path.0).unwrap();
        let replay = journal.replay().unwrap();
        assert!(replay.had_truncated_tail);
        assert_eq!(replay.records.len(), 1);
        assert_eq!(replay.records[0].payload, b"good record");
    }

    #[test]
    fn checkpoint_reset_keeps_a_valid_empty_journal() {
        let path = TestJournal::new();
        let journal = RecoveryJournal::open(&path.0).unwrap();
        journal.append(b"old snapshot").unwrap();
        journal.reset_after_checkpoint(1).unwrap()

        let replay = journal.replay().unwrap();
        assert!(!replay.had_truncated_tail);
        assert!(replay.records.is_empty());
    }

    #[test]
    fn checkpoint_sequence_filters_old_records_even_before_journal_reset() {
        let path = TestJournal::new();
        let journal = RecoveryJournal::open(&path.0).unwrap();
        assert_eq!(journal.append(b"included in checkpoint").unwrap(), 1);
        assert_eq!(journal.append(b"not included").unwrap(), 2);

        let replay = journal.replay_after(1).unwrap();
        assert_eq!(replay.records.len(), 1);
        assert_eq!(replay.records[0].sequence, 2);
        assert_eq!(replay.records[0].payload, b"not included");
    }

    #[test]
    fn reset_preserves_sequence_and_rejects_stale_checkpoint() {
        let path = TestJournal::new();
        let journal = RecoveryJournal::open(&path.0).unwrap();
        assert_eq!(journal.append(b"first").unwrap(), 1);
        assert_eq!(journal.append(b"second").unwrap(), 2);

        assert_eq!(journal.reset_after_checkpoint(1).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        journal.reset_after_checkpoint(2).unwrap();
        assert_eq!(journal.latest_sequence().unwrap(), 2);
        assert_eq!(journal.append(b"after checkpoint").unwrap(), 3);

        let replay = journal.replay().unwrap();
        assert_eq!(replay.base_sequence, 2);
        assert_eq!(replay.records[0].sequence, 3);
        assert_eq!(journal.replay_after(2).unwrap().records.len(), 1);
    }

    #[test]
    fn rejects_oversized_records() {
        let path = TestJournal::new();
        let journal = RecoveryJournal::open(&path.0).unwrap();
        let payload = vec![0; MAX_RECORD_SIZE + 1];

        assert_eq!(
            journal.append(&payload).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn replay_rejects_a_record_with_a_truncated_payload() {
        let path = TestJournal::new();
        let journal = RecoveryJournal::open(&path.0).unwrap();
        journal.append(b"payload that will be cut").unwrap();
        drop(journal);

        let file = OpenOptions::new().write(true).open(&path.0).unwrap();
        file.set_len(fs::metadata(&path.0).unwrap().len() - 3)
            .unwrap();

        let journal = RecoveryJournal::open(&path.0).unwrap();
        let replay = journal.replay().unwrap();
        assert!(replay.had_truncated_tail);
        assert!(replay.records.is_empty());
        assert_eq!(replay.valid_bytes, FILE_HEADER_SIZE);
    }

    #[test]
    fn replay_bounds_scanning_of_oversized_journal_files() {
        let path = TestJournal::new();
        let journal = RecoveryJournal::open(&path.0).unwrap();
        drop(journal);

        let file = OpenOptions::new().write(true).open(&path.0).unwrap();
        file.set_len(MAX_JOURNAL_SIZE + 1).unwrap();

        let journal = RecoveryJournal::open(&path.0).unwrap();
        let replay = journal.replay().unwrap();
        assert!(replay.had_truncated_tail);
        assert!(replay.records.is_empty());
        assert_eq!(replay.valid_bytes, FILE_HEADER_SIZE);
    }

    #[test]
    fn append_repairs_an_oversized_invalid_tail_before_writing() {
        let path = TestJournal::new();
        let journal = RecoveryJournal::open(&path.0).unwrap();
        journal.append(b"preserved").unwrap();
        drop(journal);

        let file = OpenOptions::new().write(true).open(&path.0).unwrap();
        file.set_len(MAX_JOURNAL_SIZE + 1).unwrap();

        let journal = RecoveryJournal::open(&path.0).unwrap();
        assert_eq!(journal.append(b"next").unwrap(), 2);
        let replay = journal.replay().unwrap();
        assert!(!replay.had_truncated_tail);
        assert_eq!(replay.records.len(), 2);
        assert_eq!(replay.records[0].payload, b"preserved");
        assert_eq!(replay.records[1].payload, b"next");
    }
}
