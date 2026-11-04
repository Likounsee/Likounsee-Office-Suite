# Recovery journal

The shared crate provides a binary append-only journal for autosave and crash
recovery records.

## Record format

- 8-byte file signature: LOUSJNL1.
- Each record: sequence number (u64 LE), payload length (u32 LE), CRC-32 (u32 LE),
  then payload bytes.
- Individual payloads are limited to 64 MiB.
- A journal is limited to 256 MiB total, including its header and record headers.
- Every append calls sync_data before returning. Initial journal creation also synchronizes the containing directory on Unix.

## Recovery behavior

Replay validates sequence, length and checksum while scanning incrementally in
64 KiB chunks. It does not first copy the complete journal into a second
in-memory buffer. The scan is capped at 256 MiB; an oversized or damaged tail
is flagged, while all complete valid records before it remain recoverable.

When appending, the scanner does not retain copies of old payloads: it keeps
only the last valid sequence and byte offset. The invalid/oversized tail is
truncated before a new record is written. Appends that would exceed the journal
limit are rejected, requiring the caller to persist a durable checkpoint first.

## Current constraints

- One process should own a journal path at a time; cross-process locking is not
  implemented yet.
- Replay still returns the valid payloads as owned records, so memory use is
  bounded by the journal limit (256 MiB), not constant.
- reset_after_checkpoint must only be called after the caller has independently
  verified that a durable checkpoint exists.
- The journal stores opaque bytes; document-specific serialization is not chosen.

The journal complements, rather than replaces, atomic document-file replacement.
