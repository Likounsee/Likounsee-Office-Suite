# Document engine persistence

The first document-engine persistence milestone is deliberately UI-independent.
It proves the complete local edit/recovery loop before a rich-text model or GUI
is introduced.

## Current document model

A document currently contains a UTF-8 title, UTF-8 body and monotonically
increasing revision. Its binary representation is versioned (`LOUSDOC1`) and
rejects unknown versions, malformed UTF-8, inconsistent lengths and payloads
larger than the 64 MiB journal-record limit.

Each successful title or body replacement increments the revision exactly once.
The engine journals a complete serialized document snapshot before publishing
the new in-memory state. If the append fails, the visible in-memory document is
not changed.

## Session files

A session directory contains:

- `document.checkpoint`: latest verified full-document checkpoint.
- `document.journal`: sequenced, checksummed recovery records.

On open, the engine loads and validates the checkpoint, then replays only
journal records whose sequence is newer than the checkpoint's
`journal_sequence`. Replayed document revisions must be contiguous. A journal
whose base sequence refers to a missing checkpoint is rejected rather than
silently opening an empty document.

## Autosave and checkpoint sequence

The engine uses the shared default autosave policy (2-second debounce and
15-second maximum dirty age). The UI/event loop is expected to call
`autosave_if_due(now)`; no background thread is created by the engine.

A save performs these steps:

1. Read the latest durable journal sequence.
2. Serialize the current document into a versioned checkpoint.
3. Atomically replace the checkpoint file and read it back for verification.
4. Atomically replace the journal with a header whose base sequence equals the
   checkpoint's covered sequence.
5. Mark the autosave scheduler clean only after compaction succeeds.

If the process stops after step 3 but before step 4, the next open sees the
checkpoint's covered sequence and ignores the older journal entries. Journal
compaction itself uses atomic file replacement so interruption cannot leave a
partially truncated journal header.

## Current limitations

- The model is plain text with whole-body replacement, not a rich-text editor.
- A single encoded document snapshot must fit within the journal record limit
  (64 MiB), so the current model caps total encoded document size below 64 MiB.
- Every edit writes a complete document snapshot to the journal; this is
  intentionally simple but can amplify writes for large documents.
- The autosave scheduler is polled by the host application; there is no
  background timer or GUI integration yet.
- The engine uses the local host filesystem and assumes one process owns a
  session directory. Cross-process locking is not implemented.
- A valid journal tail is replayed, but the user-facing recovery/conflict UI and
  crash-report experience are not implemented.
- This is an internal development format, not yet a stable interchange format.
