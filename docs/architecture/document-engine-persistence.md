# Document engine persistence

The document engine is UI-independent. It establishes the local edit, durable
journal, checkpoint and crash-recovery loop before a rich-text GUI is introduced.

## Current document model

A document contains a UTF-8 title, a plain-text compatibility view, a monotonically
increasing revision and an ordered list of paragraph blocks. Each paragraph has
a stable nonzero ID, UTF-8 text, a semantic style (`Normal`, `Heading1`,
`Heading2`, `Quote` or `Code`), and list metadata.

List metadata supports `None`, `Bullet` and `Numbered`, with nesting levels
from zero through eight. A paragraph without list semantics must have level zero.
The engine stores these semantics; numbering continuity, marker generation and
visual indentation are responsibilities of later layout and UI layers.

The versioned binary format (`LOUSDOC1`, v3) persists block IDs, styles, list
kinds, nesting levels and the next-ID counter. Version 1 documents are migrated
in memory into normal paragraphs; version 2 documents are migrated with no list
metadata. The decoder rejects unknown styles or list kinds, invalid list levels,
duplicate IDs, malformed UTF-8, inconsistent lengths and payloads larger than
the 64 MiB journal-record limit.

The session exposes targeted UTF-8 text operations (`insert_text`,
`delete_range` and `replace_range`) using byte offsets. It also supports
inserting and deleting paragraphs, changing paragraph styles by stable ID, and
assigning bullet or numbered list semantics. Both text-range endpoints must be
valid UTF-8 character boundaries; invalid offsets are rejected before mutation.
The engine journals a complete serialized document snapshot before publishing
the new in-memory state. If the append fails, the visible in-memory document
is not changed.

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

- Inline spans and character formatting (bold, italic, underline), tables,
  images, page layout and rendered list numbering are not implemented.
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
