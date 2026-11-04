//! Versioned, checksummed full-document checkpoints.
//!
//! Checkpoints are written through the portable atomic-replacement layer. The
//! caller must verify a newly written checkpoint before compacting its journal.

use crate::storage::FileSystem;
use crc32fast::Hasher;
use std::io;
use std::path::Path;

const MAGIC: &[u8; 8] = b"LOUSCHK1";
const FORMAT_VERSION: u32 = 1;
const HEADER_SIZE: usize = 36;
const MAX_CHECKPOINT_SIZE: usize = 128 * 1024 * 1024;

/// Maximum serialized document payload accepted by this checkpoint format.
pub const MAX_CHECKPOINT_PAYLOAD: usize = MAX_CHECKPOINT_SIZE;

/// A complete serialized document state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// Monotonically increasing generation assigned by the document owner.
    pub generation: u64,
    /// Opaque, versioned document-engine serialization.
    pub payload: Vec<u8>,
}

impl Checkpoint {
    pub fn new(generation: u64, payload: Vec<u8>) -> Self {
        Self {
            generation,
            payload,
        }
    }
}

/// Encode a checkpoint with a fixed header and CRC-32 protected payload.
pub fn encode(checkpoint: &Checkpoint) -> io::Result<Vec<u8>> {
    if checkpoint.payload.len() > MAX_CHECKPOINT_PAYLOAD {
        return Err(invalid_input("checkpoint payload exceeds 128 MiB"));
    }

    let mut bytes = Vec::with_capacity(HEADER_SIZE + checkpoint.payload.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&checkpoint.generation.to_le_bytes());
    bytes.extend_from_slice(&(checkpoint.payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&checksum(&checkpoint.payload).to_le_bytes());
    bytes.extend_from_slice(&checkpoint.payload);
    Ok(bytes)
}

/// Decode and validate a complete checkpoint. Trailing bytes are rejected.
pub fn decode(bytes: &[u8]) -> io::Result<Checkpoint> {
    if bytes.len() < HEADER_SIZE || &bytes[..8] != MAGIC {
        return Err(invalid_data("invalid checkpoint header"));
    }

    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let reserved = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    if version != FORMAT_VERSION || reserved != 0 {
        return Err(invalid_data("unsupported checkpoint format"));
    }

    let generation = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let payload_len = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    let expected_checksum = u32::from_le_bytes(bytes[32..36].try_into().unwrap());
    let payload_len = usize::try_from(payload_len)
        .map_err(|_| invalid_data("checkpoint payload length is not representable"))?;

    if payload_len > MAX_CHECKPOINT_PAYLOAD {
        return Err(invalid_data("checkpoint payload exceeds 128 MiB"));
    }

    let expected_size = HEADER_SIZE
        .checked_add(payload_len)
        .ok_or_else(|| invalid_data("checkpoint size overflow"))?;
    if bytes.len() != expected_size {
        return Err(invalid_data("checkpoint length does not match its header"));
    }

    let payload = &bytes[HEADER_SIZE..];
    if checksum(payload) != expected_checksum {
        return Err(invalid_data("checkpoint checksum mismatch"));
    }

    Ok(Checkpoint {
        generation,
        payload: payload.to_vec(),
    })
}

/// Persist a checkpoint using same-directory temporary replacement.
pub fn write_checkpoint(
    filesystem: &dyn FileSystem,
    path: &Path,
    checkpoint: &Checkpoint,
) -> io::Result<()> {
    let encoded = encode(checkpoint)?;
    filesystem.write_atomic(path, &encoded)
}

/// Load and validate a checkpoint from the selected filesystem.
pub fn read_checkpoint(filesystem: &dyn FileSystem, path: &Path) -> io::Result<Checkpoint> {
    let bytes = filesystem.read(path)?;
    decode(&bytes)
}

/// Read back a checkpoint and ensure its generation matches the expected value.
///
/// Call this after write_checkpoint and before resetting/compacting a journal.
pub fn verify_checkpoint(
    filesystem: &dyn FileSystem,
    path: &Path,
    expected_generation: u64,
) -> io::Result<Checkpoint> {
    let checkpoint = read_checkpoint(filesystem, path)?;
    if checkpoint.generation != expected_generation {
        return Err(invalid_data("checkpoint generation verification failed"));
    }
    Ok(checkpoint)
}

fn checksum(payload: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(payload);
    hasher.finalize()
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
    use crate::storage::LocalFileSystem;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "likounsee-checkpoint-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join("document.checkpoint")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn round_trips_generation_and_payload() {
        let original = Checkpoint::new(42, b"complete document state".to_vec());
        assert_eq!(decode(&encode(&original).unwrap()).unwrap(), original);
    }

    #[test]
    fn rejects_corrupt_payload() {
        let checkpoint = Checkpoint::new(1, b"document".to_vec());
        let mut encoded = encode(&checkpoint).unwrap();
        *encoded.last_mut().unwrap() ^= 0x80;
        assert_eq!(decode(&encoded).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_truncated_and_trailing_data() {
        let encoded = encode(&Checkpoint::new(1, b"abc".to_vec())).unwrap();
        assert_eq!(decode(&encoded[..encoded.len() - 1]).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let mut trailing = encoded;
        trailing.push(0);
        assert_eq!(decode(&trailing).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rejects_unknown_format_version() {
        let mut encoded = encode(&Checkpoint::new(1, b"abc".to_vec())).unwrap();
        encoded[8..12].copy_from_slice(&2_u32.to_le_bytes());
        assert_eq!(decode(&encoded).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn writes_and_verifies_checkpoint_on_local_filesystem() {
        let directory = TestDirectory::new();
        let path = directory.file();
        let filesystem = LocalFileSystem;
        let checkpoint = Checkpoint::new(7, b"saved document".to_vec());

        write_checkpoint(&filesystem, &path, &checkpoint).unwrap();

        assert_eq!(verify_checkpoint(&filesystem, &path, 7).unwrap(), checkpoint);
        assert_eq!(
            verify_checkpoint(&filesystem, &path, 8).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn failed_checkpoint_encoding_does_not_touch_existing_file() {
        let directory = TestDirectory::new();
        let path = directory.file();
        fs::write(&path, b"previous checkpoint").unwrap();
        let oversized = Checkpoint::new(2, vec![0; MAX_CHECKPOINT_PAYLOAD + 1]);

        assert_eq!(write_checkpoint(&LocalFileSystem, &path, &oversized).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(path).unwrap(), b"previous checkpoint");
    }
}
