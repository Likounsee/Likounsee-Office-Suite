# Safe file storage foundation

## Implemented in this branch

- A small FileSystem interface keeps engines independent from direct OS calls.
- LocalFileSystem supplies ordinary local file operations on Windows and Linux.
- Replacement writes create a unique temporary file in the destination directory,
  write all bytes, call sync_all, then replace the destination.
- Unix replacement uses same-directory rename and synchronizes the parent directory.
- Windows replacement uses MoveFileExW with REPLACE_EXISTING and WRITE_THROUGH.
- A failed data producer leaves the previous destination untouched and cleans up
  its temporary file.
- Existing regular-file permissions are copied to the replacement. Symbolic links
  and non-regular destinations are rejected rather than silently replaced.
- Tests cover new writes, overwrites, injected producer failure and filesystem calls.

## Important limits

This is a safer write primitive, not a claim of absolute power-loss protection.
Hardware caches, storage controllers, filesystem implementations and mount options
affect persistence. On Windows, standard file attributes are copied but custom NTFS
ACLs are not guaranteed to be preserved by this initial implementation. A directory synchronization failure can occur after replacement
has become visible; callers must report uncertain durability rather than assume the
old file is still present.

The suite must not require snapshots, NTFS, ReFS, ext4, Btrfs or OpenFS-specific
features. Filesystem-specific capabilities and guarantees still need a validation
matrix. The recovery journal will provide application-level protection independently
of filesystem snapshots.

## Next layer

Build the append-only recovery journal and autosave scheduler on this API. The
journal must detect incomplete tail records, preserve earlier valid records and
replay after an unclean application exit.
