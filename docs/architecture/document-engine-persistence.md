# Document engine persistence

The first document-engine persistence milestone is deliberately UI-independent.
It proves the complete local edit/recovery loop before a rich-text model or GUI
is introduced.

## Current document model

A document contains a UTF-8 title, a plain-text compatibility view, a monotonically
increasing revision and an ordered list of paragraph blocks. Each paragraph has
a stable nonzero ID, UTF-8 text and a semantic style (`Normal`, `Heading1`,
`Heading2`, `Quote` or `Code`). The versioned binary format (`LOUSDOC1`, v3)
persists the block IDs, styles and next-ID counter, and rejects unknown styles,
duplicate IDs, malformed UTF-8, inconsistent lengths and payloads larger than
the 64 MiB journal-record limit. Version 1 documents are still decoded and
migrated in memory as normal paragraphs.

Each successful title or body edit increments the revision exactly once. The
session exposes targeted UTF-8 text operations (insert_text, delete_range and
replace_range) using byte offsets. It also supports inserting and deleting
paragraphs and changing paragraph styles by stable ID. Both range endpoints
must be valid UTF-8 character boundaries; invalid offsets are rejected before
any mutation. The engine journals a complete serialized document snapshot
before publishing the new in-memory state. If the append fails, the visible
in-memory document is not changed.

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

- The model now has paragraph blocks and semantic paragraph styles, but it is
  not yet a full rich-text model: inline spans, character formatting, tables,
  images, lists and page layout are not implemented.
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
