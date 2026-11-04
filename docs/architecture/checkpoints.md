# Document checkpoints

A checkpoint is a complete serialized document state owned by the relevant
document engine. The shared crate treats its payload as opaque bytes.

## On-disk format

The current format uses the 8-byte signature `LOUSCHK1`, a 32-bit little-endian
format version, a reserved 32-bit field, a 64-bit generation, a 64-bit payload
length, a CRC-32 checksum, and the payload. The complete fixed header is 36
bytes. Version 1 accepts payloads up to 128 MiB and rejects unknown versions,
nonzero reserved bits, truncation, trailing bytes, and checksum failures.

## Persistence sequence

1. The document engine serializes a complete consistent document state.
2. `write_checkpoint` encodes it and delegates replacement to the portable
   atomic-write implementation.
3. The caller invokes `verify_checkpoint` to read the file back, validate its
   checksum and confirm its generation.
4. Only after successful verification may the caller compact/reset the
   corresponding recovery journal.

If writing or verification fails, the caller must retain the previous
checkpoint and recovery journal and report that saving has not completed.
The checkpoint API does not itself reset journals or claim that a document
engine's serialization is internally consistent.

## Current limitations

- Checkpoint payloads are opaque; document formats and serializers are not
  implemented yet.
- The current `FileSystem::read` interface returns a byte vector, so the
  128 MiB limit is validated after the host filesystem has read the file.
  A future bounded/streaming read API should reject oversized files before
  allocating their complete contents.
- Coordinating checkpoint generations with journal sequence numbers and
  replaying only edits newer than a checkpoint remains to be implemented.
- Durability depends on the host filesystem and storage device honoring sync
  requests. No absolute power-loss guarantee is claimed.
