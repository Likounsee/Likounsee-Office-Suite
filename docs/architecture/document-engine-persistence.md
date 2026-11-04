# Document engine persistence

The document engine is UI-independent. It establishes the local edit, durable
journal, checkpoint and crash-recovery loop before a rich-text GUI is introduced.

## Current document model

A document contains a UTF-8 title, a plain-text compatibility view, a monotonically
increasing revision and an paragraph collection and a persistent ordered sequence of paragraph and table block references. Each paragraph has
a stable nonzero ID, UTF-8 text, a semantic style (`Normal`, `Heading1`,
`Heading2`, `Quote` or `Code`), and list metadata.

List metadata supports `None`, `Bullet` and `Numbered`, with nesting levels
from zero through eight. A paragraph without list semantics must have level zero.
The engine stores these semantics; numbering continuity, marker generation and
visual indentation are responsibilities of later layout and UI layers.

The versioned binary format (`LOUSDOC1`, v6) persists paragraph IDs, table/row/cell IDs, paragraph
styles, list kinds, nesting levels, inline formatting ranges, table dimensions and the next-ID
counter. Inline marks support bold, italic and underline as independently
addressable UTF-8 byte ranges; overlapping marks of different kinds are allowed,
while ranges of the same kind are canonicalized. Version 1 documents migrate to
normal paragraphs, version 2 adds paragraph styles, version 3 adds list
metadata, and version 4 adds inline marks; older formats migrate without the
features they did not contain. Version 5 adds rectangular tables with stable
IDs for tables, rows and cells. Version 6 adds a validated block-order stream so
paragraphs and tables can be interleaved without moving or renumbering their stable IDs. The decoder rejects
unknown styles, list kinds or inline marks, invalid list levels, invalid UTF-8
boundaries, duplicate IDs, invalid or overlapping same-mark spans,
duplicate IDs, malformed UTF-8, inconsistent lengths and payloads larger than
the 64 MiB journal-record limit.

Tables and paragraphs remain stored in type-specific collections for efficient access, while `Document::blocks()` exposes their unified persistent reading order. New paragraphs can be inserted after an existing paragraph without moving adjacent tables. The legacy plain-text compatibility view still contains paragraph text only. The session can create bounded rectangular tables (up to 1,024 rows or columns and 65,536 cells per table) edit cell text by table ID and zero-based coordinates, and insert or delete rows and columns by zero-based index. Row and column edits preserve IDs of surviving rows and cells; new cells receive fresh document-wide IDs. Tables always retain at least one row and one column, and edits that exceed dimension or cell-count limits are rejected before journaling. Table cell text is UTF-8 and is persisted in checkpoints and recovery snapshots.

The session exposes targeted UTF-8 text operations (`insert_text`,
`delete_range` and `replace_range`) using byte offsets. It also supports
inserting and deleting paragraphs, changing paragraph styles by stable ID, and
assigning bullet or numbered list semantics, and applying or removing bold,
italic or underline over selected character ranges. Both text-range endpoints must be
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

- Tables are persisted, editable and interleaved with paragraphs in the block order, but table formatting and GUI rendering are not implemented.
- Images, page layout and rendered list numbering are not implemented.
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
