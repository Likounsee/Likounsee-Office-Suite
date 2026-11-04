# Recovery journal

The shared crate now provides a binary append-only journal for autosave and crash
recovery records.

## Record format

- 8-byte file signature: LOUSJNL1.
- Each record: sequence number (u64 LE), payload length (u32 LE), CRC-32 (u32 LE),
  then payload bytes.
- Individual payloads are limited to 64 MiB.
- Every append calls sync_data before returning. Initial journal creation also synchronizes the containing directory on Unix.

## Recovery behavior

Replay validates sequence, length and checksum. If the final record is incomplete
or corrupt, all earlier valid records are returned and the damaged tail is flagged.
The next append truncates only that invalid tail before writing a new sequence.
This prevents a torn write from making earlier recovery data inaccessible.

## Current constraints

- One process should own a journal path at a time; cross-process locking is not
  implemented yet.
- The journal is append-only and currently reads the journal into memory to replay.
  A bounded/streaming scanner and checkpoint compaction are future improvements.
- reset_after_checkpoint must only be called after the caller has independently
  verified that a durable checkpoint exists.
- The journal stores opaque bytes; document-specific serialization is not chosen.

The journal complements, rather than replaces, atomic document-file replacement.
