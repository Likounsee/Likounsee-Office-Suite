# Likounsee Office Suite

A free, open-source, local-first office suite aiming to provide a modern alternative to Microsoft Office and LibreOffice.

> **Project status:** Early planning. The repository currently describes the project and its intended direction; the applications and features below are goals, not claims of implemented functionality.

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

## Applications

### Document Editor

Planned capabilities:

- Rich text editing, paragraphs, styles, fonts, and page layout
- Sections, headers, footers, page numbering, and tables
- Images, shapes, lists, and tables of contents
- Search and replace, spell checking, comments, and tracked changes
- Print preview and PDF export

Target file compatibility:

- DOCX, DOC, ODT, RTF, TXT
- PDF import/export, with editing capabilities to be defined

### Spreadsheet

Planned capabilities:

- Workbooks, worksheets, cells, and data types
- Formula parser and calculation engine
- Cell references, dependency graph, and incremental recalculation
- Circular-reference detection and spreadsheet error handling
- Parallel calculation where safe and beneficial
- Formatting, sorting, filtering, conditional formatting, and data validation
- Charts and pivot tables
- Large-workbook virtualization and memory optimization
- Autosave, recovery, and stress-tested calculation

Target file compatibility:

- XLSX, XLS, ODS, CSV

### Presentation

Planned capabilities:

- Slides, layouts, themes, and templates
- Text, images, shapes, tables, and charts
- Object positioning, alignment, and ordering
- Transitions, animations, and speaker notes
- Full-screen slideshow mode
- Print and PDF export

Target file compatibility:

- PPTX, PPT, ODP

Compatibility is a goal, not a guarantee that every advanced feature in every third-party file will be supported from the first release. Importers and exporters should preserve unsupported content whenever possible rather than silently discarding it.

## Technical direction

The current language proposal is **Rust** for the applications and their processing engines. The final language and GUI framework have not yet been formally selected.

The intended architecture separates:

- Three independent application frontends
- Three application-specific document engines
- Shared components for common desktop behavior
- File-format import/export adapters
- Platform-specific integration behind portable interfaces

The GUI framework remains to be evaluated. The interface should be native-feeling, accessible, responsive, and consistent across the three applications.

## Saving, recovery, and data integrity

Preventing lost work is a core requirement, not an optional convenience.

The planned protection system includes:

1. Frequent autosave, with a short configurable interval and saves after important operations.
2. A recovery journal that records enough information to restore recent work.
3. Recovery checkpoints and retained safety versions.
4. Safe-save procedures using temporary files, validation, synchronization, and atomic replacement where the platform and filesystem support it.
5. Recovery after application crashes, operating-system failures, and unexpected power loss, subject to the guarantees of the storage hardware and filesystem.
6. Version history so users can restore an earlier state.
7. Explicit handling of failed or incomplete saves; a failed autosave must not overwrite the last known-good document.

The implementation must not promise absolute protection against every hardware failure. It must test and document the guarantees it can actually provide.

## Filesystem and operating-system compatibility

The suite should work with filesystems exposed by the operating system, rather than implementing its own filesystem access for each format.

Examples of intended environments include:

- **Windows:** NTFS, ReFS, exFAT, FAT32, and OpenFS when an appropriate Windows driver is available.
- **Linux:** ext4, Btrfs, XFS, F2FS, exFAT, and OpenFS when an appropriate Linux driver is available.

This list is a compatibility target, not a claim that every filesystem is currently supported or offers identical guarantees.

A portable filesystem abstraction should detect or account for available capabilities such as:

- Durable file synchronization
- Atomic rename/replacement behavior
- File locking and concurrent access
- Permissions and metadata
- Snapshots or copy-on-write features, when available

The suite must retain its own recovery journal and safe-save logic even when a filesystem offers snapshots or journaling. Filesystem-specific features may improve protection, but must remain optional enhancements.

## Logging and diagnostics

Each application should provide local, inspectable logs for:

- Application startup and shutdown
- File open, save, import, export, rename, and delete operations
- Autosave and recovery events
- Errors and failed operations
- Relevant diagnostic information

Logs should support rotation, configurable retention, and user-initiated export. They must not record document contents, passwords, or unnecessary personal information. Logs stay local by default and are not silently uploaded.

### Crash reporting

Crash reporting should operate only while one or more suite applications are running.

- No permanent Windows service, Linux daemon, or always-on monitoring process.
- Monitoring stops when all suite applications have closed.
- Reports may contain technical data such as application version, stack traces, and crash metadata.
- Reports must exclude document contents, passwords, and unnecessary personal information.
- Sending reports requires the user's authorization.
- If offline, reports may be queued locally for a later session, subject to the user's consent settings.

Crash handlers must remain minimal and safe; not every abrupt process termination can be intercepted.

## Updates, installation, and online services

- Applications launch directly; no always-running launcher or maintenance service is required.
- Update checks happen when an application starts, and can be disabled in settings.
- An update is downloaded and installed only after user consent.
- Updates must be integrity-checked and signed; rollback should be supported where practical.
- The suite must remain usable if the updater is disabled or the computer is offline.
- Installation, repair, update, and uninstallation run only when explicitly invoked.
- Uninstallation must not silently delete user documents.

There is no mandatory account, subscription, proprietary cloud, or synchronization service. Optional integrations with existing providers such as Google Drive, Dropbox, Nextcloud, and WebDAV may be considered later. They must never be required for local editing.

## Additional product requirements

The architecture should also leave room for:

- Printing, print preview, and page setup
- Accessibility, keyboard navigation, and screen-reader support
- Font discovery and font substitution
- Safe handling of malformed or malicious documents and macros
- Search across files and document content
- Secure scripting or macros, if introduced
- Third-party extensions with permission boundaries
- Collaboration features in a later phase
- Large documents, workbooks, and presentations
- Clean installation, repair, update, and uninstall workflows

## Development roadmap

The order below is a planning baseline and may change as implementation and testing reveal dependencies.

1. **Technical foundations** — repository structure, Rust workspace decision, GUI evaluation, build system, CI, logging, error handling, and Windows/Linux development setup.
2. **Shared components** — windows, menus, themes, settings, shortcuts, clipboard, undo/redo, file dialogs, printing interfaces, and localization.
3. **Saving and recovery infrastructure** — portable filesystem abstraction, safe writes, autosave, recovery journal, checkpoints, version history, and fault-injection tests.
4. **Document Editor** — document model, layout, editing, rendering, and file-format adapters.
5. **Spreadsheet** — workbook model, formula engine, dependency tracking, recalculation, rendering, and file-format adapters.
6. **Presentation** — slide model, rendering, editing, slideshow, and file-format adapters.
7. **Native formats and interoperability** — versioned native formats, import/export validation, and preservation of unsupported content.
8. **Consistent user experience** — accessibility, performance, customization, and cross-application consistency.
9. **Distribution** — Windows and Linux packaging, selectable application installation, updates, repair, and uninstall.
10. **Stabilization** — compatibility suites, security review, performance and memory tests, crash recovery, filesystem tests, and release preparation.
11. **Version 1.0** — first stable public release when the quality and compatibility criteria are met.

## Current status and scope

This repository is at the planning stage. No application should be considered implemented until its code, tests, and documented behavior exist.

Initial release scope remains limited to Document Editor, Spreadsheet, and Presentation. Future applications may be considered after the foundations are stable.

## Contributing

Contribution guidelines, development setup, coding conventions, architecture decisions, and issue templates will be added as the codebase takes shape.

Until then, proposals should focus on the three initial applications, cross-platform reliability, interoperability, privacy, and preventing data loss.

## License

The project intends to be free and open source. The exact license must be selected and added before the first code contributions are published.
