//! Portable local-file access and crash-conscious replacement writes.
//!
//! The API deliberately exposes ordinary paths and byte buffers. It does not
//! assume NTFS, ReFS, ext4, Btrfs, OpenFS, or snapshot support.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Minimal filesystem interface used by engines and persistence components.
/// Implementations can be substituted in tests to inject I/O failures.
pub trait FileSystem: Send + Sync {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn write_atomic(&self, path: &Path, contents: &[u8]) -> io::Result<()>;
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn exists(&self, path: &Path) -> bool;
}

/// Filesystem implementation backed by the host operating system.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalFileSystem;

impl FileSystem for LocalFileSystem {
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn write_atomic(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        write_atomic(path, contents)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

/// Write a complete replacement without truncating the previous file first.
///
/// The temporary file is created in the destination directory, written and
/// synchronized before replacement. Unix also synchronizes the parent directory.
/// A failure during final directory synchronization means the replacement may
/// already be visible while crash-durability is uncertain.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    write_atomic_with(path, |file| file.write_all(contents))
}

/// Stream data into a temporary file. If the producer fails before replacement,
/// the original destination remains untouched and the temporary file is removed.
pub fn write_atomic_with<F>(path: &Path, producer: F) -> io::Result<()>
where
    F: FnOnce(&mut File) -> io::Result<()>,
{
    let parent = destination_parent(path)?;
    if path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination must name a file",
        ));
    }

    let existing_permissions = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "destination is a symbolic link; resolve it before atomic replacement",
            ));
        }
        Ok(metadata) if metadata.is_file() => Some(metadata.permissions()),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "destination exists but is not a regular file",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };

    let (temp_path, mut temp_file) = create_temporary_file(parent)?;
    let cleanup = TempFileCleanup(temp_path.clone());

    producer(&mut temp_file)?;
    if let Some(permissions) = existing_permissions {
        temp_file.set_permissions(permissions)?;
    }
    temp_file.sync_all()?;
    drop(temp_file);
    replace_file(&temp_path, path)?;
    drop(cleanup); // The temporary path no longer exists after a successful replacement.
    sync_parent_directory(parent)?;
    Ok(())
}

fn destination_parent(path: &Path) -> io::Result<&Path> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => Ok(parent),
        _ => Ok(Path::new(".")),
    }
}

fn create_temporary_file(parent: &Path) -> io::Result<(PathBuf, File)> {
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_name = format!(".likounsee-tmp-{}-{sequence}", std::process::id());
        let temp_path = parent.join(temp_name);

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary file after 128 attempts",
    ))
}

struct TempFileCleanup(PathBuf);

impl Drop for TempFileCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();

    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };

    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock must be after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "likounsee-office-storage-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn writes_new_file() {
        let dir = TestDirectory::new();
        let target = dir.file("document.bin");
        write_atomic(&target, b"first version").expect("atomic write should succeed");
        assert_eq!(fs::read(target).unwrap(), b"first version");
    }

    #[test]
    fn replaces_existing_file() {
        let dir = TestDirectory::new();
        let target = dir.file("document.bin");
        fs::write(&target, b"old valid content").unwrap();
        write_atomic(&target, b"new complete content").expect("replacement should succeed");
        assert_eq!(fs::read(target).unwrap(), b"new complete content");
    }

    #[test]
    fn producer_failure_preserves_old_file_and_cleans_temporary_file() {
        let dir = TestDirectory::new();
        let target = dir.file("document.bin");
        fs::write(&target, b"old valid content").unwrap();
        let result = write_atomic_with(&target, |file| {
            file.write_all(b"incomplete replacement")?;
            Err(io::Error::other("simulated interrupted save"))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old valid content");
        let leftovers: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".likounsee-tmp-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn preserves_existing_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TestDirectory::new();
        let target = dir.file("private.bin");
        fs::write(&target, b"private").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();

        write_atomic(&target, b"updated").unwrap();

        assert_eq!(
            fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_replace_a_symbolic_link() {
        use std::os::unix::fs::symlink;

        let dir = TestDirectory::new();
        let target = dir.file("real.bin");
        let link = dir.file("link.bin");
        fs::write(&target, b"original").unwrap();
        symlink(&target, &link).unwrap();

        let error = write_atomic(&link, b"replacement").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    }

    #[test]
    fn local_filesystem_supports_portable_operations() {
        let dir = TestDirectory::new();
        let target = dir.file("nested/data.bin");
        let fs_api = LocalFileSystem;
        fs_api.create_dir_all(target.parent().unwrap()).unwrap();
        fs_api.write_atomic(&target, b"portable").unwrap();
        assert!(fs_api.exists(&target));
        assert_eq!(fs_api.read(&target).unwrap(), b"portable");
        fs_api.remove_file(&target).unwrap();
        assert!(!fs_api.exists(&target));
    }
}
