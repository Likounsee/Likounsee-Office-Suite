//! Append-only recovery journal for autosave snapshots or edit batches.
//!
//! Each record has a monotonically increasing sequence, bounded payload length
//! and CRC-32. Replay stops at the first incomplete or invalid tail record.

use crc32fast::Hasher;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const FILE_MAGIC: &[u8; 8] = b"LOUSJNL1";
const RECORD_HEADER_SIZE: usize = 16;
const MAX_RECORD_SIZE: usize = 64 * 1024 * 1024;

/// One valid journal entry recovered from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    pub sequence: u64,
    pub payload: Vec<u8>,
}

/// Result of scanning a journal. Valid records before a damaged tail are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalReplay {
    pub records: Vec<JournalRecord>,
    pub had_truncated_tail: bool,
    pub valid_bytes: u64,
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
        let mut file = OpenOptions::new().read(true).write(true).create(true).open(&path)?;
        if file.metadata()?.len() == 0 {
            file.write_all(FILE_MAGIC)?;
            file.sync_all()?;
        } else {
            let mut magic = [0_u8; 8];
            file.read_exact(&mut magic)?;
            if &magic != FILE_MAGIC {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid recovery journal header"));
            }
        }
        Ok(Self { path, gate: Mutex::new(()) })
    }

    /// Append one record and synchronously flush it to the storage device.
    pub fn append(&self, payload: &[u8]) -> io::Result<u64> {
        if payload.len() > MAX_RECORD_SIZE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "recovery record exceeds 64 MiB"));
        }
        let _guard = self.gate.lock()
            .map_err(|_| io::Error::other("recovery journal lock was poisoned"))?;
        let mut file = OpenOptions::new().read(true).write(true).open(&self.path)?;
        let replay = scan(&mut file)?;
        file.set_len(replay.valid_bytes)?;
        file.seek(SeekFrom::Start(replay.valid_bytes))?;

        let sequence = replay.records.last().map_or(1, |record| record.sequence + 1);
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
        let _guard = self.gate.lock()
            .map_err(|_| io::Error::other("recovery journal lock was poisoned"))?;
        let mut file = File::open(&self.path)?;
        scan(&mut file)
    }

    /// Remove all records after a caller has safely persisted a checkpoint.
    pub fn reset_after_checkpoint(&self) -> io::Result<()> {
        let _guard = self.gate.lock()
            .map_err(|_| io::Error::other("recovery journal lock was poisoned"))?;
        let mut file = OpenOptions::new().write(true).truncate(true).open(&self.path)?;
        file.write_all(FILE_MAGIC)?;
        file.sync_all()
    }

    /// Return the path used by this journal.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn scan(file: &mut File) -> io::Result<JournalReplay> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.len() < FILE_MAGIC.len() || &bytes[..FILE_MAGIC.len()] != FILE_MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid recovery journal header"));
    }

    let mut offset = FILE_MAGIC.len();
    let mut records = Vec::new();
    let mut expected_sequence = 1_u64;
    let mut had_truncated_tail = false;

    while offset < bytes.len() {
        if bytes.len() - offset < RECORD_HEADER_SIZE {
            had_truncated_tail = true;
            break;
        }
        let sequence = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
        let length = u32::from_le_bytes(bytes[offset + 8..offset + 12].try_into().unwrap()) as usize;
        let expected_checksum = u32::from_le_bytes(bytes[offset + 12..offset + 16].try_into().unwrap());
        if length > MAX_RECORD_SIZE || sequence != expected_sequence
            || bytes.len() - offset - RECORD_HEADER_SIZE < length
        {
            had_truncated_tail = true;
            break;
        }
        let payload_start = offset + RECORD_HEADER_SIZE;
        let payload_end = payload_start + length;
        let payload = &bytes[payload_start..payload_end];
        if checksum(payload) != expected_checksum {
            had_truncated_tail = true;
            break;
        }
        records.push(JournalRecord { sequence, payload: payload.to_vec() });
        expected_sequence += 1;
        offset = payload_end;
    }

    Ok(JournalReplay {
        records,
        had_truncated_tail,
        valid_bytes: offset as u64,
    })
}

fn checksum(payload: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(payload);
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestJournal(PathBuf);
    impl TestJournal {
        fn new() -> Self {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("likounsee-journal-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir.join("recovery.journal"))
        }
    }
    impl Drop for TestJournal {
        fn drop(&mut self) {
            if let Some(parent) = self.0.parent() { let _ = fs::remove_dir_all(parent); }
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
        assert_eq!(replay.records, vec![
            JournalRecord { sequence: 1, payload: b"snapshot one".to_vec() },
            JournalRecord { sequence: 2, payload: b"snapshot two".to_vec() },
        ]);
    }

    #[test]
    fn replay_keeps_valid_records_before_a_torn_tail() {
        let path = TestJournal::new();
        let journal = RecoveryJournal::open(&path.0).unwrap();
        journal.append(b"valid").unwrap();
        drop(journal);
        OpenOptions::new().append(true).open(&path.0).unwrap()
            .write_all(&[2, 0, 0, 0]).unwrap();
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
        OpenOptions::new().append(true).open(&path.0).unwrap()
            .write_all(&[9, 9, 9]).unwrap();
        let journal = RecoveryJournal::open(&path.0).unwrap();
        assert_eq!(journal.append(b"second").unwrap(), 2);
        let replay = journal.replay().unwrap();
        assert!(!replay.had_truncated_tail);
        assert_eq!(replay.records.len(), 2);
        assert_eq!(replay.records[1].payload, b"second");
    }

    #[test]
    fn rejects_oversized_records() {
        let path = TestJournal::new();
        let journal = RecoveryJournal::open(&path.0).unwrap();
        let payload = vec![0; MAX_RECORD_SIZE + 1];
        assert_eq!(journal.append(&payload).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
}
