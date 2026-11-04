# Likounsee Office Suite

A free, open-source, local-first office suite aiming to provide a modern alternative to Microsoft Office and LibreOffice.

> **Project status:** Initial Rust foundation in progress on the `foundation/initial-architecture` branch. The workspace, three app/engine boundaries, shared errors/logging, portable file storage, safer replacement writes, a checksummed recovery journal and Windows/Linux CI have been added. The actual office features are not implemented yet.

## Vision

Build a professional desktop office suite with a modern, consistent interface, strong file compatibility, reliable recovery, and support for Windows and Linux.

The initial scope is deliberately limited to three applications:

| Application | Purpose | Inspiration |
|---|---|---|
| **Document Editor** | Create and edit text documents | Microsoft Word |
| **Spreadsheet** | Workbooks, formulas, data and charts | Microsoft Excel |
| **Presentation** | Slideshows and presentations | Microsoft PowerPoint |

Email, note-taking, database management, chat, and other applications are outside the initial scope.

## Project principles

- **Free and open source:** no mandatory subscription or paid account.
- **Local-first:** documents are stored locally by default.
- **Offline-capable:** core editing, saving, and document access must work without an Internet connection.
- **Cross-platform:** Windows and Linux are primary targets.
- **Filesystem-independent:** applications must not depend on NTFS-specific behavior.
- **Privacy by design:** no mandatory telemetry, cloud account, or remote storage.
- **Data-loss prevention:** frequent autosave, recovery journals, safe writes, and version history are architectural priorities.
- **Extensible:** the architecture should allow future applications and third-party extensions without expanding the initial product scope.

## Technical direction

The implementation language is **Rust**. Windows and Linux are first-class targets from the start. The GUI framework is deliberately undecided until a documented comparison and cross-platform proof of concept are complete.

The intended architecture separates three application frontends, three independent engines, shared components, import/export adapters, and platform integration behind portable interfaces. Engines remain independent of any GUI toolkit.

## Saving and recovery

The initial persistence layer now includes:

- A portable filesystem interface with a local Windows/Linux implementation.
- Safer replacement writes using a same-directory temporary file, file synchronization and platform-specific replacement.
- An append-only recovery journal with monotonically increasing record numbers, CRC-32 validation and detection of incomplete/corrupt tail records.
- A thread-free autosave scheduler: save after 2 seconds of inactivity or after 15 seconds of continuous unsaved work by default.
- Tests for replacement, simulated write failure, journal replay, torn-tail recovery and autosave deadlines.

These are foundations, not a complete autosave feature. Document-specific journal serialization, checkpoint management, compaction, concurrent-process locking and filesystem-specific validation are still to be implemented. The scheduler defines when to save; the applications still need to connect it to their document models. No software can guarantee survival of every hardware or power failure.

## File compatibility goals

- **Document Editor:** DOCX, DOC, ODT, RTF, TXT and PDF import/export.
- **Spreadsheet:** XLSX, XLS, ODS and CSV.
- **Presentation:** PPTX, PPT, ODP and PDF export.

Compatibility is a goal, not a claim that every advanced third-party feature is supported. Unsupported content should be preserved whenever possible.

## Development and architecture

See:
- CONTRIBUTING.md
- docs/development/setup.md
- docs/architecture/overview.md
- docs/architecture/gui-framework-evaluation.md
- docs/architecture/safe-file-storage.md
- docs/architecture/recovery-journal.md

## Current scope

The three application executables are still startup stubs. Editing, calculation, presentation design, import/export, autosave scheduling and version history are not implemented. The GUI toolkit remains undecided pending the cross-platform proof of concept.
