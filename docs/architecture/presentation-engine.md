# Presentation engine

The presentation engine is a UI-independent Rust model. It is independent from the desktop
application and can be used by a future presentation editor, command-line tool or conversion layer.

## Current model

- Presentation title and custom page dimensions, expressed in points.
- Ordered slides with stable IDs, names, 24-bit RGB backgrounds and speaker notes.
- Slide creation, duplication, deletion and reordering.
- Text elements with geometry, font family, size, color, bold, italic, underline and text alignment.
- Shape elements: rectangle, ellipse, line and arrow, with fill, stroke and width.
- Image elements with a source reference and alternative text. Image bytes are not embedded or decoded.
- Element geometry updates, element reordering and deletion.
- Multi-selection alignment against the collective bounding box or the slide canvas (left, horizontal center, right, top, vertical center and bottom).
- Even horizontal or vertical distribution of three or more selected elements.
- Text content/style updates, shape style updates, image source/alt-text updates.
- Global presentation themes with title/body typography, slide backgrounds and accent colors, plus built-in Light, Dark, Business and High contrast presets. Applying a theme updates all existing slides atomically while preserving text emphasis/alignment, geometry, shape strokes, notes and image references; the resulting styles are persisted by the normal presentation serializer.
- Model validation, stable IDs, bounded collection/string sizes.
- Versioned binary serialization with CRC32 integrity checking.
- Local session journal, crash recovery, autosave scheduling and checkpoint compaction.

## Session and durability

The session edit operation applies one mutation to a cloned presentation, validates it,
appends the encoded snapshot to the recovery journal, and only then publishes the new in-memory
model. A failed operation or journal append does not publish a partial edit. Autosave is polled
by the host application; save writes and verifies a checkpoint before compacting the journal.

## Current limitations

- No presentation GUI, canvas renderer or slideshow playback yet.
- No PPTX import/export; the current binary format is internal.
- Image sources are references only; decoding, embedding and media packaging are not implemented.
- Animations, transitions, master slides, charts, audio/video, comments and collaboration are not implemented. Themes currently apply directly to existing model styles; named theme metadata is not retained as a separately editable theme object.
- Text layout, font fallback, wrapping and rendering belong to a future layout/rendering layer.
- No clipboard integration, undo/redo history or accessibility tree yet.

## Relationship to the document engine

The document engine is not a dependency of the presentation engine. They have different models
and persistence formats. Reusable low-level pieces such as autosave, checkpoints, recovery journals
and filesystem abstractions already come from suite-common. Text styling concepts can inform a
shared text-layout component later, but document paragraphs and tables should not be embedded
directly into slides. A dedicated shared text/layout layer should be extracted once its contracts
are stable.
