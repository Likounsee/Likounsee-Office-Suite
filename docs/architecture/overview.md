# Architecture overview

## Confirmed decisions
- Language: Rust.
- First-class desktop targets: Windows and Linux.
- GUI framework: undecided; evaluation required before selection.
- Initial apps: Document Editor, Spreadsheet and Presentation.

## Workspace boundaries
- `apps/*`: thin application entry points and platform integration.
- `engines/*`: UI-independent document, spreadsheet and presentation logic.
- `crates/suite-common`: shared primitives used by apps and engines.

Engines must be testable without a window. Parsing, layout, recalculation,
rendering and file operations must not block the UI thread once a GUI exists.

## Planned foundations (not implemented yet)
- Portable filesystem abstraction and safe-save operations.
- Autosave scheduler, recovery journal and version checkpoints.
- Import/export adapters for OOXML, ODF, legacy Office formats and PDF.
- Diagnostic logging and crash-report consent flow.
- Shared UI components, localization and accessibility.

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
