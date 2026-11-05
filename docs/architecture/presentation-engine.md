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
- Global presentation themes with title/body typography, slide backgrounds and accent colors, plus built-in Light, Dark, Business and High contrast presets. Built-in slide layouts create editable text placeholders for title-only, comparison, quote and main-content/sidebar compositions. Native SVG rendering is available per slide (`render_slide_svg`) and for an entire deck (`render_all_slides_svg`). It exports slide backgrounds, text styling, word-wrapped and clipped text blocks, rectangle/ellipse/line/arrow shapes and externally referenced images; image bytes are not embedded. PDF export is available per slide (`render_slide_pdf`) and for the full presentation (`render_presentation_pdf`), using vector text and shapes without introducing a shared graphics engine or external rendering dependency. PNG export is exposed through `render_slide_png_with` and `render_presentation_png_with`, which accept an application-provided `PngRasterizer`; the presentation engine only supplies SVG and validates the returned PNG signature, chunk framing and types, CRCs, IHDR bit-depth/color-type/compression/filter/interlace values, palette placement, consecutive IDAT chunks, image-data presence and requested dimensions. It contains no rasterizer implementation and adds no graphics dependency, so a separate graphics engine can implement the adapter. PDF uses the 12 built-in PDF Type 1 fonts across Helvetica, Times and Courier families, with regular/bold/italic variants. Common family aliases such as Arial, Times New Roman, Georgia, Consolas and Courier map to those portable fonts; unknown families fall back to Helvetica. WinAnsi text encoding is used and unsupported Unicode glyphs are replaced with `?`. Arbitrary fonts are not embedded. PDF image elements can be embedded when the host application supplies already encoded JPEG bytes through `PdfImageProvider`; the engine validates JPEG frame dimensions/component count and a 64 MiB size limit, and writes a PDF DCTDecode image XObject without decoding or rasterizing. CMYK JPEGs receive the PDF inverted-component decode array. Repeated references share one image object. Missing images retain a visible placeholder and alternative text. The original PDF export functions remain available without a provider. PNG decoding, arbitrary image formats and media packaging remain outside this engine. SVG/PDF wrapping uses a deterministic character-width estimate rather than font shaping, so it is a portable first-pass layout, not a typography engine. Applying a theme updates all existing slides atomically while preserving text emphasis/alignment, geometry, shape strokes, notes and image references; the resulting styles are persisted by the normal presentation serializer.
- Model validation, stable IDs, bounded collection/string sizes.
- Versioned binary serialization with CRC32 integrity checking.
- Local session journal, crash recovery, autosave scheduling and checkpoint compaction.

## Session and durability

The session edit operation applies one mutation to a cloned presentation, validates it,
appends the encoded snapshot to the recovery journal, and only then publishes the new in-memory
model. A failed operation or journal append does not publish a partial edit. Autosave is polled
by the host application; save writes and verifies a checkpoint before compacting the journal.

## Current limitations

- No presentation GUI, interactive canvas or slideshow playback yet. SVG/PDF are native export formats; PNG requires an external `PngRasterizer` implementation. This crate does not contain a rasterizer or general-purpose graphics engine.
- A first PPTX export subset is available through `export_pptx`: it writes a ZIP/OOXML package with editable text and primitive shapes, slide dimensions, backgrounds, and a minimal master/layout/theme. Image references are preserved as relinkable placeholders, not embedded media; speaker notes, PPTX import, animations, transitions, charts and advanced layout features are not yet supported.
- SVG keeps image references external. PDF can embed host-supplied JPEG bytes through `PdfImageProvider`. PPTX currently emits an editable placeholder containing the image alternative text and source reference; the host application must resolve and package actual image media in a later step.
- Animations, transitions, master slides, charts, audio/video, comments and collaboration are not implemented. Themes currently apply directly to existing model styles; named theme metadata is not retained as a separately editable theme object.
- Accurate font shaping, arbitrary font embedding, advanced text layout and Unicode font fallback are not implemented; PDF maps common font families to the built-in Helvetica/Times/Courier families and current SVG/PDF exports use approximate wrapping.
- No clipboard integration, undo/redo history or accessibility tree yet.

## Relationship to the document engine

The document engine is not a dependency of the presentation engine. They have different models
and persistence formats. Reusable low-level pieces such as autosave, checkpoints, recovery journals
and filesystem abstractions already come from suite-common. Text styling concepts can inform a
shared text-layout component later, but document paragraphs and tables should not be embedded
directly into slides. A dedicated shared text/layout layer should be extracted once its contracts
are stable.
