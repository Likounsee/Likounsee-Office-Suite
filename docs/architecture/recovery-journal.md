# Recovery journal

The shared crate provides a binary append-only journal for autosave and crash
recovery records.

## File and record format

- 8-byte file signature: `LOUSJNL2`.
- 64-bit little-endian base sequence in the file header (initially zero).
- Each record: sequence number (u64 LE), payload length (u32 LE), CRC-32 (u32 LE),
  then payload bytes.
- Individual payloads are limited to 64 MiB.
- A journal is limited to 256 MiB total, including its 16-byte file header and
  record headers.
- Every append calls sync_data before returning. Initial journal creation and
  checkpoint compaction synchronize the containing directory on Unix.

## Recovery behavior

Replay validates sequence, length and checksum while scanning incrementally in
64 KiB chunks. It does not first copy the complete journal into a second
in-memory buffer. The scan is capped at 256 MiB; an oversized or damaged tail
is flagged, while all complete valid records before it remain recoverable.

The first record after the base sequence must have sequence
`base_sequence + 1`. `latest_sequence()` exposes the latest valid sequence,
and `replay_after(checkpoint_sequence)` filters records already included in a
verified checkpoint. This filtering is safe even if a crash occurred after the
checkpoint was persisted but before the old journal was compacted.

`reset_after_checkpoint(sequence)` accepts only the latest valid sequence,
then writes it as the new base sequence. The next append continues at
`sequence + 1`; sequence numbers never restart at one after compaction. A
stale checkpoint cannot erase records appended after its snapshot.

When appending, the scanner does not retain copies of old payloads: it keeps
only the last valid sequence and byte offset. The invalid/oversized tail is
truncated before a new record is written. Appends that would exceed the journal
limit are rejected, requiring the caller to persist a durable checkpoint first.

## Current constraints

- Journal format version 2 (`LOUSJNL2`) is not backward-compatible with the
  unreleased version 1 development format.
- One process should own a journal path at a time; cross-process locking is not
  implemented yet.
- Replay still returns valid payloads as owned records, so memory use is
  bounded by the journal limit (256 MiB), not constant.
- The journal stores opaque bytes; document-specific serialization is not chosen.

The journal complements, rather than replaces, atomic document-file replacement.
