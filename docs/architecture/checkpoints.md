# Document checkpoints

A checkpoint is a complete serialized document state owned by the relevant
document engine. The shared crate treats its payload as opaque bytes.

## On-disk format

Format version 2 uses the 8-byte signature `LOUSCHK1`, a 32-bit little-endian
format version, a reserved 32-bit field, a 64-bit checkpoint generation, a
64-bit recovery-journal sequence covered by the snapshot, a 64-bit payload
length, a CRC-32 checksum, and the payload. The fixed header is 44 bytes.
Version 2 accepts payloads up to 128 MiB and rejects unknown versions,
nonzero reserved bits, truncation, trailing bytes, and checksum failures.

The journal sequence is distinct from the checkpoint generation: it records
exactly which durable edit-log entries are already represented by the document
snapshot.

## Persistence and crash recovery

1. The document owner obtains a consistent document snapshot and the latest
   journal sequence represented by that snapshot.
2. `write_checkpoint` encodes the state and delegates replacement to the
   portable atomic-write implementation.
3. The caller invokes `verify_checkpoint` to read the file back, validate its
   checksum and confirm its generation.
4. Recovery always uses `replay_after(checkpoint.journal_sequence)`, including
   when the old journal has not yet been reset. Records at or below the stored
   sequence are already represented in the checkpoint and are ignored.
5. Once the checkpoint is verified, `reset_after_checkpoint(sequence)` may
   compact the journal. It accepts only the latest valid journal sequence and
   writes that sequence as the new journal base. New records continue at
   `sequence + 1`.

This ordering handles a crash after checkpoint replacement but before journal
reset: the checkpoint carries the covered sequence, so replay skips old
records. If new edits were appended after the snapshot, reset refuses to erase
them; replay_after keeps those newer records available.

If writing or verification fails, the caller must retain the previous
checkpoint and recovery journal and report that saving has not completed.
The checkpoint API does not itself guarantee that a document engine's
serialization and journal sequence were captured under one consistent lock.

## Current limitations

- Checkpoint payloads are opaque; document formats and serializers are not
  implemented yet.
- The current `FileSystem::read` interface returns a byte vector, so the
  128 MiB limit is validated after the host filesystem has read the file.
  A future bounded/streaming read API should reject oversized files before
  allocating their complete contents.
- Durability depends on the host filesystem and storage device honoring sync
  requests. No absolute power-loss guarantee is claimed.
