# GUI framework evaluation

**Status: open — no framework selected.**

Candidates to investigate: Slint, egui, iced, GTK4 (gtk-rs) and Qt 6 Rust
bindings. This is a shortlist, not a ranking. Verify current releases, licensing,
accessibility and platform support before scoring.

## Weighted scorecard
Score each candidate from 1 (poor) to 5 (excellent), with evidence and tested
version recorded:
- Windows/Linux parity and packaging: 20%
- Accessibility, keyboard navigation and screen readers: 15%
- Rich text editing and typography: 15%
- Large spreadsheet/grid performance: 15%
- Professional theming and customization: 10%
- Printing, clipboard, drag-and-drop and file dialogs: 10%
- Rust integration, stability and maintenance: 10%
- Licensing and deployment footprint: 5%

## Required proof of concept
1. Main window, menus, toolbar and collapsible side panel.
2. Rich-text editing, selection, clipboard and keyboard navigation.
3. Virtualized grid with at least 100,000 cells.
4. Slide canvas, zoom and selection handles.
5. High-DPI, dark/light themes and font fallback.
6. Accessibility and screen-reader checks on both operating systems.
7. Print preview, native dialogs and drag-and-drop.
8. Startup, memory, scrolling and input-latency measurements.
9. Clean packaging, install and uninstall on Windows and Linux.

Choose only after the same prototype has been evaluated against the scorecard.
Keep document models, calculations and persistence independent of the toolkit.
