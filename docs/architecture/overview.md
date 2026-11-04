# Architecture overview

## Confirmed decisions
- Language: Rust.
- First-class desktop targets: Windows and Linux.
- GUI framework: undecided; evaluation required before selection.
- Initial apps: Document Editor, Spreadsheet and Presentation.

## Workspace boundaries
- apps/*: thin application entry points and platform integration.
- engines/*: UI-independent document, spreadsheet and presentation logic.
- crates/suite-common: shared primitives used by apps and engines.

Engines must be testable without a window. Parsing, layout, recalculation,
rendering and file operations must not block the UI thread once a GUI exists.

## Implemented foundations on foundation/initial-architecture
- Shared error handling and process-local logging.
- Portable FileSystem interface and local filesystem implementation.
- Safer replacement writes using same-directory temporary files, synchronization
  and platform-specific replacement primitives.
- Append-only recovery journal with sequence validation, CRC-32 and torn-tail
  detection/recovery.
- Windows/Linux CI workflow for formatting, linting, tests and builds.

## Implemented persistence primitives
- Autosave scheduling policy (2-second debounce, 15-second maximum dirty age by default).
- Recovery checkpoints and journal compaction are implemented as shared primitives\n  and are now integrated into the first document-engine persistence session.\n- Document editing remains a minimal text model; rich-text structures, document\n  formats, collaboration and UI integration are not implemented yet.
- Filesystem capability validation matrix across real filesystems.
- Import/export adapters for OOXML, ODF, legacy Office formats and PDF.
- Crash-report consent flow and shared UI components.

Correct local editing must not depend on cloud access, filesystem snapshots or
filesystem-specific features. Detect and document durability capabilities rather
than assuming identical behavior across filesystems.

## Dependency direction
Apps may depend on their engine and shared primitives. Engines may depend on
shared primitives. Shared primitives must not depend on apps, engines or a GUI.

## Compatibility policy
Windows and Linux are first-class CI targets. NTFS, ReFS, FAT32, exFAT, ext4,
Btrfs, XFS and future OpenFS support are targets for validation, not guarantees
provided by this initial scaffold.
