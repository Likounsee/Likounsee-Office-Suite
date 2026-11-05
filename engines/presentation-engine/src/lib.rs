//! UI-independent presentation model, validation and versioned persistence.
//!
//! Coordinates are expressed in points. Image sources are references (paths or
//! URIs); the engine does not decode or embed image bytes yet.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use suite_common::autosave::AutosaveScheduler;
use suite_common::checkpoint::{Checkpoint, read_checkpoint, verify_checkpoint, write_checkpoint};
use suite_common::recovery::RecoveryJournal;
use suite_common::storage::{FileSystem, LocalFileSystem};

const PRESENTATION_MAGIC: &[u8; 8] = b"LOUSPRS1";
const PRESENTATION_VERSION: u32 = 1;
const MAX_PRESENTATION_BYTES: usize = 64 * 1024 * 1024;
const MAX_SLIDES: usize = 10_000;
const MAX_ELEMENTS_PER_SLIDE: usize = 100_000;
const MAX_STRING_BYTES: usize = 16 * 1024 * 1024;

pub const fn engine_name() -> &'static str {
    "presentation-engine"
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> io::Result<Self> {
        let value = Self {
            x,
            y,
            width,
            height,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn x(&self) -> f32 {
        self.x
    }
    pub fn y(&self) -> f32 {
        self.y
    }
    pub fn width(&self) -> f32 {
        self.width
    }
    pub fn height(&self) -> f32 {
        self.height
    }
    fn validate(&self) -> io::Result<()> {
        if !self.x.is_finite()
            || !self.y.is_finite()
            || !self.width.is_finite()
            || !self.height.is_finite()
            || self.width <= 0.0
            || self.height <= 0.0
            || self.x.abs() > 1_000_000.0
            || self.y.abs() > 1_000_000.0
            || self.width > 1_000_000.0
            || self.height > 1_000_000.0
        {
            return Err(invalid_input("element bounds are invalid"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum TextAlignment {
    #[default]
    Left = 0,
    Center = 1,
    Right = 2,
    Justify = 3,
}
impl TextAlignment {
    fn decode(value: u8) -> io::Result<Self> {
        match value {
            0 => Ok(Self::Left),
            1 => Ok(Self::Center),
            2 => Ok(Self::Right),
            3 => Ok(Self::Justify),
            _ => Err(invalid_data("unknown text alignment")),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    font_family: String,
    font_size: f32,
    color: u32,
    bold: bool,
    italic: bool,
    underline: bool,
    alignment: TextAlignment,
}
impl Default for TextStyle {
    fn default() -> Self {
        Self {
            font_family: "Arial".to_owned(),
            font_size: 24.0,
            color: 0x000000,
            bold: false,
            italic: false,
            underline: false,
            alignment: TextAlignment::Left,
        }
    }
}
impl TextStyle {
    pub fn new(font_family: impl Into<String>, font_size: f32, color: u32) -> io::Result<Self> {
        let style = Self {
            font_family: font_family.into(),
            font_size,
            color,
            ..Self::default()
        };
        style.validate()?;
        Ok(style)
    }
    pub fn font_family(&self) -> &str {
        &self.font_family
    }
    pub fn font_size(&self) -> f32 {
        self.font_size
    }
    pub fn color(&self) -> u32 {
        self.color
    }
    pub fn bold(&self) -> bool {
        self.bold
    }
    pub fn italic(&self) -> bool {
        self.italic
    }
    pub fn underline(&self) -> bool {
        self.underline
    }
    pub fn alignment(&self) -> TextAlignment {
        self.alignment
    }
    pub fn set_emphasis(&mut self, bold: bool, italic: bool, underline: bool) {
        self.bold = bold;
        self.italic = italic;
        self.underline = underline;
    }
    pub fn set_alignment(&mut self, alignment: TextAlignment) {
        self.alignment = alignment;
    }
    fn validate(&self) -> io::Result<()> {
        validate_string(&self.font_family, "font family")?;
        if self.font_family.is_empty()
            || !self.font_size.is_finite()
            || !(1.0..=1000.0).contains(&self.font_size)
            || self.color > 0x00ff_ffff
        {
            return Err(invalid_input("text style is invalid"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ShapeKind {
    Rectangle = 0,
    Ellipse = 1,
    Line = 2,
    Arrow = 3,
}
impl ShapeKind {
    fn decode(value: u8) -> io::Result<Self> {
        match value {
            0 => Ok(Self::Rectangle),
            1 => Ok(Self::Ellipse),
            2 => Ok(Self::Line),
            3 => Ok(Self::Arrow),
            _ => Err(invalid_data("unknown shape kind")),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElementKind {
    Text {
        text: String,
        style: TextStyle,
    },
    Shape {
        shape: ShapeKind,
        fill: Option<u32>,
        stroke: Option<u32>,
        stroke_width: f32,
    },
    Image {
        source: String,
        alt_text: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SlideElement {
    id: u64,
    bounds: Rect,
    kind: ElementKind,
}
impl SlideElement {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn bounds(&self) -> Rect {
        self.bounds
    }
    pub fn kind(&self) -> &ElementKind {
        &self.kind
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Slide {
    id: u64,
    name: String,
    background: u32,
    notes: String,
    elements: Vec<SlideElement>,
}
impl Slide {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn background(&self) -> u32 {
        self.background
    }
    pub fn notes(&self) -> &str {
        &self.notes
    }
    pub fn elements(&self) -> &[SlideElement] {
        &self.elements
    }
    pub fn element(&self, id: u64) -> Option<&SlideElement> {
        self.elements.iter().find(|element| element.id == id)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Presentation {
    title: String,
    width: f32,
    height: f32,
    revision: u64,
    slides: Vec<Slide>,
    next_id: u64,
}
impl Presentation {
    /// Create a presentation with one blank slide. Dimensions are in points.
    pub fn new(title: impl Into<String>, width: f32, height: f32) -> io::Result<Self> {
        let title = title.into();
        validate_string(&title, "presentation title")?;
        validate_dimensions(width, height)?;
        Ok(Self {
            title,
            width,
            height,
            revision: 0,
            slides: vec![Slide {
                id: 1,
                name: "Slide 1".to_owned(),
                background: 0xffffff,
                notes: String::new(),
                elements: Vec::new(),
            }],
            next_id: 2,
        })
    }
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn width(&self) -> f32 {
        self.width
    }
    pub fn height(&self) -> f32 {
        self.height
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn slides(&self) -> &[Slide] {
        &self.slides
    }
    pub fn slide(&self, id: u64) -> Option<&Slide> {
        self.slides.iter().find(|slide| slide.id == id)
    }

    fn bump_revision(&mut self) -> io::Result<()> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid_input("presentation revision exhausted"))?;
        Ok(())
    }
    fn take_id(&mut self) -> io::Result<u64> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| invalid_input("presentation ID space exhausted"))?;
        Ok(id)
    }
    fn slide_mut(&mut self, id: u64) -> io::Result<&mut Slide> {
        self.slides
            .iter_mut()
            .find(|slide| slide.id == id)
            .ok_or_else(|| invalid_input("slide does not exist"))
    }
    pub fn set_title(&mut self, title: impl Into<String>) -> io::Result<()> {
        let title = title.into();
        validate_string(&title, "presentation title")?;
        self.bump_revision()?;
        self.title = title;
        Ok(())
    }
    pub fn add_slide(&mut self, name: impl Into<String>) -> io::Result<u64> {
        if self.slides.len() >= MAX_SLIDES {
            return Err(invalid_input("slide limit reached"));
        }
        let name = name.into();
        validate_string(&name, "slide name")?;
        self.bump_revision()?;
        let id = self.take_id()?;
        self.slides.push(Slide {
            id,
            name,
            background: 0xffffff,
            notes: String::new(),
            elements: Vec::new(),
        });
        Ok(id)
    }
    pub fn duplicate_slide(&mut self, slide_id: u64) -> io::Result<u64> {
        let source = self
            .slide(slide_id)
            .cloned()
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        if self.slides.len() >= MAX_SLIDES {
            return Err(invalid_input("slide limit reached"));
        }
        self.bump_revision()?;
        let mut duplicate = source;
        duplicate.id = self.take_id()?;
        duplicate.name.push_str(" copy");
        for element in &mut duplicate.elements {
            element.id = self.take_id()?;
        }
        let id = duplicate.id;
        let index = self
            .slides
            .iter()
            .position(|slide| slide.id == slide_id)
            .unwrap()
            + 1;
        self.slides.insert(index, duplicate);
        Ok(id)
    }
    pub fn remove_slide(&mut self, slide_id: u64) -> io::Result<()> {
        if self.slides.len() == 1 {
            return Err(invalid_input("a presentation must keep at least one slide"));
        }
        let index = self
            .slides
            .iter()
            .position(|slide| slide.id == slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        self.bump_revision()?;
        self.slides.remove(index);
        Ok(())
    }
    pub fn move_slide(&mut self, slide_id: u64, new_index: usize) -> io::Result<()> {
        if new_index >= self.slides.len() {
            return Err(invalid_input("slide index is out of range"));
        }
        let old_index = self
            .slides
            .iter()
            .position(|slide| slide.id == slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        if old_index == new_index {
            return Ok(());
        }
        self.bump_revision()?;
        let slide = self.slides.remove(old_index);
        self.slides.insert(new_index, slide);
        Ok(())
    }
    pub fn set_slide_background(&mut self, slide_id: u64, color: u32) -> io::Result<()> {
        validate_color(color)?;
        if self.slide(slide_id).is_none() {
            return Err(invalid_input("slide does not exist"));
        }
        self.bump_revision()?;
        self.slide_mut(slide_id)?.background = color;
        Ok(())
    }
    pub fn set_slide_notes(&mut self, slide_id: u64, notes: impl Into<String>) -> io::Result<()> {
        let notes = notes.into();
        validate_string(&notes, "slide notes")?;
        if self.slide(slide_id).is_none() {
            return Err(invalid_input("slide does not exist"));
        }
        self.bump_revision()?;
        self.slide_mut(slide_id)?.notes = notes;
        Ok(())
    }

    /// Rename a slide without changing its stable identity.
    pub fn rename_slide(&mut self, slide_id: u64, name: impl Into<String>) -> io::Result<()> {
        let name = name.into();
        validate_string(&name, "slide name")?;
        if self.slide(slide_id).is_none() {
            return Err(invalid_input("slide does not exist"));
        }
        self.bump_revision()?;
        self.slide_mut(slide_id)?.name = name;
        Ok(())
    }

    /// Replace the complete text style of an existing text element.
    pub fn set_text_style(
        &mut self,
        slide_id: u64,
        element_id: u64,
        style: TextStyle,
    ) -> io::Result<()> {
        style.validate()?;
        let slide = self.slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        if !matches!(slide.element(element_id).map(|element| &element.kind), Some(ElementKind::Text { .. })) {
            return Err(invalid_input("text element does not exist"));
        }
        self.bump_revision()?;
        let element = self.slide_mut(slide_id)?.elements.iter_mut()
            .find(|element| element.id == element_id).unwrap();
        if let ElementKind::Text { style: current, .. } = &mut element.kind {
            *current = style;
        }
        Ok(())
    }

    /// Change fill, stroke and stroke width on an existing shape.
    pub fn set_shape_style(
        &mut self,
        slide_id: u64,
        element_id: u64,
        fill: Option<u32>,
        stroke: Option<u32>,
        stroke_width: f32,
    ) -> io::Result<()> {
        if fill.is_some_and(|color| color > 0x00ff_ffff)
            || stroke.is_some_and(|color| color > 0x00ff_ffff)
            || !stroke_width.is_finite()
            || !(0.0..=1000.0).contains(&stroke_width)
        {
            return Err(invalid_input("shape styling is invalid"));
        }
        let slide = self.slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        if !matches!(slide.element(element_id).map(|element| &element.kind), Some(ElementKind::Shape { .. })) {
            return Err(invalid_input("shape element does not exist"));
        }
        self.bump_revision()?;
        let element = self.slide_mut(slide_id)?.elements.iter_mut()
            .find(|element| element.id == element_id).unwrap();
        if let ElementKind::Shape { fill: current_fill, stroke: current_stroke, stroke_width: current_width, .. } = &mut element.kind {
            *current_fill = fill;
            *current_stroke = stroke;
            *current_width = stroke_width;
        }
        Ok(())
    }

    /// Replace the source and alternative text of an existing image element.
    pub fn update_image(
        &mut self,
        slide_id: u64,
        element_id: u64,
        source: impl Into<String>,
        alt_text: impl Into<String>,
    ) -> io::Result<()> {
        let source = source.into();
        let alt_text = alt_text.into();
        validate_string(&source, "image source")?;
        validate_string(&alt_text, "image alt text")?;
        if source.is_empty() {
            return Err(invalid_input("image source cannot be empty"));
        }
        let slide = self.slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        if !matches!(slide.element(element_id).map(|element| &element.kind), Some(ElementKind::Image { .. })) {
            return Err(invalid_input("image element does not exist"));
        }
        self.bump_revision()?;
        let element = self.slide_mut(slide_id)?.elements.iter_mut()
            .find(|element| element.id == element_id).unwrap();
        if let ElementKind::Image { source: current_source, alt_text: current_alt } = &mut element.kind {
            *current_source = source;
            *current_alt = alt_text;
        }
        Ok(())
    }
    pub fn add_text(
        &mut self,
        slide_id: u64,
        bounds: Rect,
        text: impl Into<String>,
        style: TextStyle,
    ) -> io::Result<u64> {
        bounds.validate()?;
        style.validate()?;
        let text = text.into();
        validate_string(&text, "text content")?;
        self.ensure_element_capacity(slide_id)?;
        self.bump_revision()?;
        let id = self.take_id()?;
        self.slide_mut(slide_id)?.elements.push(SlideElement {
            id,
            bounds,
            kind: ElementKind::Text { text, style },
        });
        Ok(id)
    }
    pub fn add_shape(
        &mut self,
        slide_id: u64,
        bounds: Rect,
        shape: ShapeKind,
        fill: Option<u32>,
        stroke: Option<u32>,
        stroke_width: f32,
    ) -> io::Result<u64> {
        bounds.validate()?;
        if fill.is_some_and(|c| c > 0x00ff_ffff)
            || stroke.is_some_and(|c| c > 0x00ff_ffff)
            || !stroke_width.is_finite()
            || !(0.0..=1000.0).contains(&stroke_width)
        {
            return Err(invalid_input("shape styling is invalid"));
        }
        self.ensure_element_capacity(slide_id)?;
        self.bump_revision()?;
        let id = self.take_id()?;
        self.slide_mut(slide_id)?.elements.push(SlideElement {
            id,
            bounds,
            kind: ElementKind::Shape {
                shape,
                fill,
                stroke,
                stroke_width,
            },
        });
        Ok(id)
    }
    pub fn add_image(
        &mut self,
        slide_id: u64,
        bounds: Rect,
        source: impl Into<String>,
        alt_text: impl Into<String>,
    ) -> io::Result<u64> {
        bounds.validate()?;
        let source = source.into();
        let alt_text = alt_text.into();
        validate_string(&source, "image source")?;
        validate_string(&alt_text, "image alt text")?;
        if source.is_empty() {
            return Err(invalid_input("image source cannot be empty"));
        }
        self.ensure_element_capacity(slide_id)?;
        self.bump_revision()?;
        let id = self.take_id()?;
        self.slide_mut(slide_id)?.elements.push(SlideElement {
            id,
            bounds,
            kind: ElementKind::Image { source, alt_text },
        });
        Ok(id)
    }
    fn ensure_element_capacity(&self, slide_id: u64) -> io::Result<()> {
        let slide = self
            .slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        if slide.elements.len() >= MAX_ELEMENTS_PER_SLIDE {
            return Err(invalid_input("slide element limit reached"));
        }
        Ok(())
    }
    pub fn update_text(
        &mut self,
        slide_id: u64,
        element_id: u64,
        text: impl Into<String>,
    ) -> io::Result<()> {
        let text = text.into();
        validate_string(&text, "text content")?;
        let slide = self
            .slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        if !matches!(
            slide.element(element_id).map(|e| &e.kind),
            Some(ElementKind::Text { .. })
        ) {
            return Err(invalid_input("text element does not exist"));
        }
        self.bump_revision()?;
        let element = self
            .slide_mut(slide_id)?
            .elements
            .iter_mut()
            .find(|e| e.id == element_id)
            .unwrap();
        if let ElementKind::Text { text: current, .. } = &mut element.kind {
            *current = text;
        }
        Ok(())
    }
    pub fn set_element_bounds(
        &mut self,
        slide_id: u64,
        element_id: u64,
        bounds: Rect,
    ) -> io::Result<()> {
        bounds.validate()?;
        let slide = self
            .slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        if slide.element(element_id).is_none() {
            return Err(invalid_input("element does not exist"));
        }
        self.bump_revision()?;
        self.slide_mut(slide_id)?
            .elements
            .iter_mut()
            .find(|e| e.id == element_id)
            .unwrap()
            .bounds = bounds;
        Ok(())
    }
    pub fn move_element(
        &mut self,
        slide_id: u64,
        element_id: u64,
        new_index: usize,
    ) -> io::Result<()> {
        let slide = self
            .slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        if new_index >= slide.elements.len() {
            return Err(invalid_input("element index is out of range"));
        }
        let old_index = slide
            .elements
            .iter()
            .position(|e| e.id == element_id)
            .ok_or_else(|| invalid_input("element does not exist"))?;
        if old_index == new_index {
            return Ok(());
        }
        self.bump_revision()?;
        let elements = &mut self.slide_mut(slide_id)?.elements;
        let element = elements.remove(old_index);
        elements.insert(new_index, element);
        Ok(())
    }
    pub fn remove_element(&mut self, slide_id: u64, element_id: u64) -> io::Result<()> {
        let slide = self
            .slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        let index = slide
            .elements
            .iter()
            .position(|e| e.id == element_id)
            .ok_or_else(|| invalid_input("element does not exist"))?;
        self.bump_revision()?;
        self.slide_mut(slide_id)?.elements.remove(index);
        Ok(())
    }
    pub fn validate(&self) -> io::Result<()> {
        validate_string(&self.title, "presentation title")?;
        validate_dimensions(self.width, self.height)?;
        if self.slides.is_empty() || self.slides.len() > MAX_SLIDES {
            return Err(invalid_input("slide count is invalid"));
        }
        let mut ids = std::collections::HashSet::new();
        let mut max_id = 0;
        for slide in &self.slides {
            if slide.id == 0 || !ids.insert(slide.id) {
                return Err(invalid_input("slide IDs must be unique and nonzero"));
            }
            max_id = max_id.max(slide.id);
            validate_string(&slide.name, "slide name")?;
            validate_string(&slide.notes, "slide notes")?;
            validate_color(slide.background)?;
            if slide.elements.len() > MAX_ELEMENTS_PER_SLIDE {
                return Err(invalid_input("slide element limit exceeded"));
            }
            for element in &slide.elements {
                if element.id == 0 || !ids.insert(element.id) {
                    return Err(invalid_input("element IDs must be unique and nonzero"));
                }
                max_id = max_id.max(element.id);
                element.bounds.validate()?;
                match &element.kind {
                    ElementKind::Text { text, style } => {
                        validate_string(text, "text content")?;
                        style.validate()?;
                    }
                    ElementKind::Shape {
                        fill,
                        stroke,
                        stroke_width,
                        ..
                    } => {
                        if fill.is_some_and(|c| c > 0x00ff_ffff)
                            || stroke.is_some_and(|c| c > 0x00ff_ffff)
                            || !stroke_width.is_finite()
                            || !(0.0..=1000.0).contains(stroke_width)
                        {
                            return Err(invalid_input("shape styling is invalid"));
                        }
                    }
                    ElementKind::Image { source, alt_text } => {
                        validate_string(source, "image source")?;
                        validate_string(alt_text, "image alt text")?;
                        if source.is_empty() {
                            return Err(invalid_input("image source cannot be empty"));
                        }
                    }
                }
            }
        }
        if self.next_id <= max_id {
            return Err(invalid_input(
                "next ID must exceed every slide and element ID",
            ));
        }
        Ok(())
    }
}

pub fn encode_presentation(presentation: &Presentation) -> io::Result<Vec<u8>> {
    presentation.validate()?;
    let mut out = Vec::new();
    out.extend_from_slice(PRESENTATION_MAGIC);
    put_u32(&mut out, PRESENTATION_VERSION)?;
    put_u64(&mut out, presentation.revision);
    put_f32(&mut out, presentation.width);
    put_f32(&mut out, presentation.height);
    put_u64(&mut out, presentation.next_id);
    put_string(&mut out, &presentation.title)?;
    put_count(&mut out, presentation.slides.len())?;
    for slide in &presentation.slides {
        put_u64(&mut out, slide.id);
        put_string(&mut out, &slide.name)?;
        put_u32(&mut out, slide.background)?;
        put_string(&mut out, &slide.notes)?;
        put_count(&mut out, slide.elements.len())?;
        for element in &slide.elements {
            put_u64(&mut out, element.id);
            put_f32(&mut out, element.bounds.x);
            put_f32(&mut out, element.bounds.y);
            put_f32(&mut out, element.bounds.width);
            put_f32(&mut out, element.bounds.height);
            match &element.kind {
                ElementKind::Text { text, style } => {
                    out.push(0);
                    put_string(&mut out, text)?;
                    put_string(&mut out, &style.font_family)?;
                    put_f32(&mut out, style.font_size);
                    put_u32(&mut out, style.color)?;
                    out.push(u8::from(style.bold));
                    out.push(u8::from(style.italic));
                    out.push(u8::from(style.underline));
                    out.push(style.alignment as u8);
                }
                ElementKind::Shape {
                    shape,
                    fill,
                    stroke,
                    stroke_width,
                } => {
                    out.push(1);
                    out.push(*shape as u8);
                    put_option_color(&mut out, *fill);
                    put_option_color(&mut out, *stroke);
                    put_f32(&mut out, *stroke_width);
                }
                ElementKind::Image { source, alt_text } => {
                    out.push(2);
                    put_string(&mut out, source)?;
                    put_string(&mut out, alt_text)?;
                }
            }
            if out.len() > MAX_PRESENTATION_BYTES {
                return Err(invalid_input("presentation exceeds 64 MiB"));
            }
        }
    }
    if out.len() > MAX_PRESENTATION_BYTES - 4 {
        return Err(invalid_input("presentation exceeds 64 MiB"));
    }
    let checksum = crc32fast::hash(&out);
    put_u32(&mut out, checksum)?;
    Ok(out)
}

pub fn decode_presentation(bytes: &[u8]) -> io::Result<Presentation> {
    if bytes.len() < 40 || bytes.len() > MAX_PRESENTATION_BYTES || &bytes[..8] != PRESENTATION_MAGIC
    {
        return Err(invalid_data("invalid presentation header or size"));
    }
    let payload_len = bytes.len() - 4;
    let expected = u32::from_le_bytes(bytes[payload_len..].try_into().unwrap());
    if crc32fast::hash(&bytes[..payload_len]) != expected {
        return Err(invalid_data("presentation checksum mismatch"));
    }
    let mut reader = Reader {
        bytes: &bytes[..payload_len],
        cursor: 8,
    };
    if reader.u32()? != PRESENTATION_VERSION {
        return Err(invalid_data("unsupported presentation version"));
    }
    let revision = reader.u64()?;
    let width = reader.f32()?;
    let height = reader.f32()?;
    validate_dimensions(width, height).map_err(|_| invalid_data("invalid slide dimensions"))?;
    let next_id = reader.u64()?;
    let title = reader.string()?;
    let slide_count = reader.count(MAX_SLIDES)?;
    if slide_count == 0 {
        return Err(invalid_data("presentation must contain a slide"));
    }
    let mut slides = Vec::with_capacity(slide_count);
    for _ in 0..slide_count {
        let id = reader.u64()?;
        let name = reader.string()?;
        let background = reader.u32()?;
        let notes = reader.string()?;
        let element_count = reader.count(MAX_ELEMENTS_PER_SLIDE)?;
        let mut elements = Vec::with_capacity(element_count);
        for _ in 0..element_count {
            let id = reader.u64()?;
            let bounds = Rect {
                x: reader.f32()?,
                y: reader.f32()?,
                width: reader.f32()?,
                height: reader.f32()?,
            };
            let kind = match reader.byte()? {
                0 => {
                    let text = reader.string()?;
                    let font_family = reader.string()?;
                    let font_size = reader.f32()?;
                    let color = reader.u32()?;
                    let bold = reader.boolean()?;
                    let italic = reader.boolean()?;
                    let underline = reader.boolean()?;
                    let alignment = TextAlignment::decode(reader.byte()?)?;
                    ElementKind::Text {
                        text,
                        style: TextStyle {
                            font_family,
                            font_size,
                            color,
                            bold,
                            italic,
                            underline,
                            alignment,
                        },
                    }
                }
                1 => {
                    let shape = ShapeKind::decode(reader.byte()?)?;
                    let fill = reader.option_color()?;
                    let stroke = reader.option_color()?;
                    let stroke_width = reader.f32()?;
                    ElementKind::Shape {
                        shape,
                        fill,
                        stroke,
                        stroke_width,
                    }
                }
                2 => ElementKind::Image {
                    source: reader.string()?,
                    alt_text: reader.string()?,
                },
                _ => return Err(invalid_data("unknown presentation element type")),
            };
            elements.push(SlideElement { id, bounds, kind });
        }
        slides.push(Slide {
            id,
            name,
            background,
            notes,
            elements,
        });
    }
    if reader.cursor != payload_len {
        return Err(invalid_data("presentation contains trailing data"));
    }
    let presentation = Presentation {
        title,
        width,
        height,
        revision,
        slides,
        next_id,
    };
    presentation
        .validate()
        .map_err(|_| invalid_data("presentation model is invalid"))?;
    Ok(presentation)
}

fn validate_dimensions(width: f32, height: f32) -> io::Result<()> {
    if !width.is_finite()
        || !height.is_finite()
        || !(1.0..=20_000.0).contains(&width)
        || !(1.0..=20_000.0).contains(&height)
    {
        return Err(invalid_input(
            "slide dimensions must be finite and between 1 and 20000 points",
        ));
    }
    Ok(())
}
fn validate_color(color: u32) -> io::Result<()> {
    if color > 0x00ff_ffff {
        Err(invalid_input("color must be 24-bit RGB"))
    } else {
        Ok(())
    }
}
fn validate_string(value: &str, label: &str) -> io::Result<()> {
    if value.len() > MAX_STRING_BYTES {
        Err(invalid_input(format!("{label} exceeds 16 MiB")))
    } else {
        Ok(())
    }
}
fn put_u32(out: &mut Vec<u8>, value: u32) -> io::Result<()> {
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}
fn put_count(out: &mut Vec<u8>, value: usize) -> io::Result<()> {
    let value = u32::try_from(value)
        .map_err(|_| invalid_input("collection length is not representable"))?;
    put_u32(out, value)
}
fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_f32(out: &mut Vec<u8>, value: f32) {
    out.extend_from_slice(&value.to_bits().to_le_bytes());
}
fn put_string(out: &mut Vec<u8>, value: &str) -> io::Result<()> {
    validate_string(value, "string")?;
    put_count(out, value.len())?;
    out.extend_from_slice(value.as_bytes());
    Ok(())
}
fn put_option_color(out: &mut Vec<u8>, value: Option<u32>) {
    match value {
        Some(color) => {
            out.push(1);
            out.extend_from_slice(&color.to_le_bytes());
        }
        None => out.push(0),
    }
}
fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

struct Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}
impl Reader<'_> {
    fn take(&mut self, length: usize) -> io::Result<&[u8]> {
        let end = self
            .cursor
            .checked_add(length)
            .ok_or_else(|| invalid_data("presentation offset overflow"))?;
        if end > self.bytes.len() {
            return Err(invalid_data("truncated presentation"));
        }
        let value = &self.bytes[self.cursor..end];
        self.cursor = end;
        Ok(value)
    }
    fn byte(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn boolean(&mut self) -> io::Result<bool> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid_data("invalid boolean value")),
        }
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> io::Result<f32> {
        Ok(f32::from_bits(u32::from_le_bytes(
            self.take(4)?.try_into().unwrap(),
        )))
    }
    fn string(&mut self) -> io::Result<String> {
        let length = self.count(MAX_STRING_BYTES)?;
        String::from_utf8(self.take(length)?.to_vec())
            .map_err(|_| invalid_data("presentation string is not UTF-8"))
    }
    fn count(&mut self, maximum: usize) -> io::Result<usize> {
        let count = self.u32()? as usize;
        if count > maximum {
            return Err(invalid_data("presentation collection exceeds its limit"));
        }
        Ok(count)
    }
    fn option_color(&mut self) -> io::Result<Option<u32>> {
        match self.byte()? {
            0 => Ok(None),
            1 => Ok(Some(self.u32()?)),
            _ => Err(invalid_data("invalid optional color")),
        }
    }
}

const CHECKPOINT_FILE: &str = "presentation.checkpoint";
const JOURNAL_FILE: &str = "presentation.journal";

/// Crash-recoverable presentation session. Each edit is journaled before it
/// becomes visible in memory; checkpoints compact the journal on save.
pub struct PresentationSession {
    filesystem: LocalFileSystem,
    directory: PathBuf,
    presentation: Presentation,
    journal: RecoveryJournal,
    autosave: AutosaveScheduler,
    checkpoint_generation: u64,
    recovered_records: usize,
    journal_had_truncated_tail: bool,
}

impl PresentationSession {
    pub fn open(directory: impl AsRef<Path>) -> io::Result<Self> {
        let directory = directory.as_ref().to_path_buf();
        let filesystem = LocalFileSystem;
        filesystem.create_dir_all(&directory)?;
        let checkpoint_path = directory.join(CHECKPOINT_FILE);
        let journal = RecoveryJournal::open(directory.join(JOURNAL_FILE))?;
        let checkpoint = if filesystem.exists(&checkpoint_path) {
            Some(read_checkpoint(&filesystem, &checkpoint_path)?)
        } else {
            None
        };
        let (mut presentation, checkpoint_generation, replay) = if let Some(checkpoint) = checkpoint
        {
            let replay = journal.replay_after(checkpoint.journal_sequence)?;
            (
                decode_presentation(&checkpoint.payload)?,
                checkpoint.generation,
                replay,
            )
        } else {
            let replay = journal.replay()?;
            if replay.base_sequence != 0 {
                return Err(invalid_data(
                    "journal has a checkpoint base sequence but checkpoint is missing",
                ));
            }
            (Presentation::new("Untitled", 960.0, 540.0)?, 0, replay)
        };
        let recovered_records = replay.records.len();
        let journal_had_truncated_tail = replay.had_truncated_tail;
        for record in replay.records {
            let recovered = decode_presentation(&record.payload)?;
            let expected_revision = presentation
                .revision
                .checked_add(1)
                .ok_or_else(|| invalid_data("presentation revision exhausted during recovery"))?;
            if recovered.revision != expected_revision {
                return Err(invalid_data(
                    "presentation journal revisions are not contiguous",
                ));
            }
            presentation = recovered;
        }
        let mut autosave = AutosaveScheduler::default();
        if recovered_records > 0 {
            autosave.mark_dirty(Instant::now());
        }
        Ok(Self {
            filesystem,
            directory,
            presentation,
            journal,
            autosave,
            checkpoint_generation,
            recovered_records,
            journal_had_truncated_tail,
        })
    }

    pub fn presentation(&self) -> &Presentation {
        &self.presentation
    }
    pub fn recovered_records(&self) -> usize {
        self.recovered_records
    }
    pub fn journal_had_truncated_tail(&self) -> bool {
        self.journal_had_truncated_tail
    }
    pub fn is_dirty(&self) -> bool {
        self.autosave.is_dirty()
    }

    /// Apply one model operation transactionally and append its snapshot.
    pub fn edit<T>(
        &mut self,
        operation: impl FnOnce(&mut Presentation) -> io::Result<T>,
    ) -> io::Result<T> {
        let mut next = self.presentation.clone();
        let result = operation(&mut next)?;
        next.validate()?;
        let expected_revision = self
            .presentation
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid_input("presentation revision exhausted"))?;
        if next.revision != expected_revision {
            return Err(invalid_input(
                "a session edit must perform exactly one presentation mutation",
            ));
        }
        let encoded = encode_presentation(&next)?;
        self.journal.append(&encoded)?;
        self.presentation = next;
        self.autosave.mark_dirty(Instant::now());
        Ok(result)
    }

    pub fn autosave_if_due(&mut self, now: Instant) -> io::Result<bool> {
        if !self.autosave.should_save(now) {
            return Ok(false);
        }
        self.save_now()?;
        Ok(true)
    }

    pub fn save_now(&mut self) -> io::Result<()> {
        if !self.autosave.is_dirty() && self.filesystem.exists(&self.checkpoint_path()) {
            return Ok(());
        }
        let generation = self
            .checkpoint_generation
            .checked_add(1)
            .ok_or_else(|| invalid_input("checkpoint generation exhausted"))?;
        let sequence = self.journal.latest_sequence()?;
        let checkpoint = Checkpoint::new(
            generation,
            sequence,
            encode_presentation(&self.presentation)?,
        );
        let path = self.checkpoint_path();
        write_checkpoint(&self.filesystem, &path, &checkpoint)?;
        verify_checkpoint(&self.filesystem, &path, generation)?;
        self.journal.reset_after_checkpoint(sequence)?;
        self.checkpoint_generation = generation;
        self.autosave.mark_saved();
        self.recovered_records = 0;
        self.journal_had_truncated_tail = false;
        Ok(())
    }

    fn checkpoint_path(&self) -> PathBuf {
        self.directory.join(CHECKPOINT_FILE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect::new(x, y, width, height).unwrap()
    }

    #[test]
    fn session_journals_edits_and_recovers_after_reopen() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let path = std::env::temp_dir().join(format!(
            "likounsee-presentation-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let mut session = PresentationSession::open(&path).unwrap();
            let slide = session.presentation().slides()[0].id();
            session
                .edit(|deck| deck.add_slide("Recovered slide"))
                .unwrap();
            session
                .edit(|deck| {
                    deck.add_text(
                        slide,
                        rect(10.0, 10.0, 200.0, 50.0),
                        "Journaled",
                        TextStyle::default(),
                    )
                })
                .unwrap();
            assert!(session.is_dirty());
        }
        {
            let session = PresentationSession::open(&path).unwrap();
            assert_eq!(session.presentation().slides().len(), 2);
            assert_eq!(session.presentation().slides()[0].elements().len(), 1);
            assert_eq!(session.recovered_records(), 2);
        }
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn session_rejects_failed_edits_without_committing_them() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let path = std::env::temp_dir().join(format!(
            "likounsee-presentation-rollback-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut session = PresentationSession::open(&path).unwrap();
        let revision = session.presentation().revision();
        let result = session.edit(|deck| deck.remove_slide(deck.slides()[0].id()));
        assert!(result.is_err());
        assert_eq!(session.presentation().revision(), revision);
        assert!(!session.is_dirty());
        drop(session);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn session_checkpoint_compacts_journal_and_reopens_cleanly() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let path = std::env::temp_dir().join(format!(
            "likounsee-presentation-checkpoint-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let mut session = PresentationSession::open(&path).unwrap();
            session.edit(|deck| deck.add_slide("Saved")).unwrap();
            session.save_now().unwrap();
            assert!(!session.is_dirty());
            assert_eq!(session.recovered_records(), 0);
        }
        {
            let session = PresentationSession::open(&path).unwrap();
            assert_eq!(session.presentation().slides().len(), 2);
            assert_eq!(session.recovered_records(), 0);
        }
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn engine_has_a_stable_name() {
        assert_eq!(engine_name(), "presentation-engine");
    }

    #[test]
    fn new_presentation_starts_with_one_blank_slide() {
        let deck = Presentation::new("Quarterly", 960.0, 540.0).unwrap();
        assert_eq!(deck.slides().len(), 1);
        assert_eq!(deck.slides()[0].name(), "Slide 1");
        assert_eq!(deck.revision(), 0);
    }

    #[test]
    fn rejects_invalid_page_and_element_geometry() {
        assert!(Presentation::new("bad", f32::NAN, 540.0).is_err());
        assert!(Rect::new(0.0, 0.0, 0.0, 10.0).is_err());
        assert!(Rect::new(f32::INFINITY, 0.0, 10.0, 10.0).is_err());
    }

    #[test]
    fn adds_and_duplicates_slides_with_fresh_element_ids() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let first = deck.slides()[0].id();
        let element = deck
            .add_text(
                first,
                rect(10.0, 20.0, 300.0, 80.0),
                "Title",
                TextStyle::default(),
            )
            .unwrap();
        let duplicate = deck.duplicate_slide(first).unwrap();
        assert_eq!(deck.slides().len(), 2);
        assert_ne!(deck.slide(duplicate).unwrap().elements()[0].id(), element);
        assert_eq!(
            deck.slide(duplicate).unwrap().elements()[0].kind(),
            deck.slide(first).unwrap().elements()[0].kind()
        );
    }

    #[test]
    fn moves_and_removes_slides_without_losing_identity() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let first = deck.slides()[0].id();
        let second = deck.add_slide("Second").unwrap();
        let third = deck.add_slide("Third").unwrap();
        deck.move_slide(third, 0).unwrap();
        assert_eq!(deck.slides()[0].id(), third);
        deck.remove_slide(second).unwrap();
        assert!(deck.slide(second).is_none());
        assert_eq!(deck.slides().len(), 2);
        assert!(deck.remove_slide(first).is_ok());
        assert!(deck.remove_slide(third).is_err());
    }

    #[test]
    fn supports_text_shape_image_notes_and_background() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let slide = deck.slides()[0].id();
        let mut style = TextStyle::new("Inter", 32.0, 0x112233).unwrap();
        style.set_emphasis(true, true, false);
        style.set_alignment(TextAlignment::Center);
        let text = deck
            .add_text(slide, rect(20.0, 20.0, 400.0, 90.0), "Hello", style)
            .unwrap();
        let shape = deck
            .add_shape(
                slide,
                rect(30.0, 130.0, 200.0, 100.0),
                ShapeKind::Ellipse,
                Some(0xabcdef),
                Some(0x000000),
                2.0,
            )
            .unwrap();
        let image = deck
            .add_image(
                slide,
                rect(300.0, 130.0, 200.0, 100.0),
                "assets/photo.png",
                "A photo",
            )
            .unwrap();
        deck.set_slide_background(slide, 0xfefefe).unwrap();
        deck.set_slide_notes(slide, "Presenter notes").unwrap();
        assert_eq!(deck.slide(slide).unwrap().elements().len(), 3);
        assert_eq!(deck.slide(slide).unwrap().element(text).unwrap().id(), text);
        assert!(deck.slide(slide).unwrap().element(shape).is_some());
        assert!(deck.slide(slide).unwrap().element(image).is_some());
        assert_eq!(deck.slide(slide).unwrap().notes(), "Presenter notes");
    }

    #[test]
    fn edits_and_reorders_elements() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let slide = deck.slides()[0].id();
        let first = deck
            .add_text(
                slide,
                rect(0.0, 0.0, 100.0, 40.0),
                "A",
                TextStyle::default(),
            )
            .unwrap();
        let second = deck
            .add_text(
                slide,
                rect(0.0, 50.0, 100.0, 40.0),
                "B",
                TextStyle::default(),
            )
            .unwrap();
        deck.update_text(slide, first, "Updated").unwrap();
        deck.set_element_bounds(slide, first, rect(5.0, 5.0, 120.0, 50.0))
            .unwrap();
        deck.move_element(slide, second, 0).unwrap();
        assert_eq!(deck.slide(slide).unwrap().elements()[0].id(), second);
        deck.remove_element(slide, first).unwrap();
        assert!(deck.slide(slide).unwrap().element(first).is_none());
    }

    #[test]
    fn updates_slide_names_text_style_shape_style_and_image_metadata() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let slide = deck.slides()[0].id();
        deck.rename_slide(slide, "Introduction").unwrap();
        let text = deck.add_text(slide, rect(0.0, 0.0, 100.0, 40.0), "Title", TextStyle::default()).unwrap();
        let shape = deck.add_shape(slide, rect(0.0, 50.0, 100.0, 40.0), ShapeKind::Rectangle, None, None, 1.0).unwrap();
        let image = deck.add_image(slide, rect(0.0, 100.0, 100.0, 80.0), "old.png", "Old").unwrap();

        let mut style = TextStyle::new("Inter", 36.0, 0x123456).unwrap();
        style.set_emphasis(true, false, true);
        style.set_alignment(TextAlignment::Center);
        deck.set_text_style(slide, text, style.clone()).unwrap();
        deck.set_shape_style(slide, shape, Some(0xaabbcc), Some(0x010203), 3.5).unwrap();
        deck.update_image(slide, image, "new.png", "Updated image").unwrap();

        assert_eq!(deck.slide(slide).unwrap().name(), "Introduction");
        assert!(matches!(deck.slide(slide).unwrap().element(text).unwrap().kind(), ElementKind::Text { style: actual, .. } if actual == &style));
        assert!(matches!(deck.slide(slide).unwrap().element(shape).unwrap().kind(), ElementKind::Shape { fill: Some(0xaabbcc), stroke: Some(0x010203), stroke_width, .. } if *stroke_width == 3.5));
        assert!(matches!(deck.slide(slide).unwrap().element(image).unwrap().kind(), ElementKind::Image { source, alt_text } if source == "new.png" && alt_text == "Updated image"));

        let restored = decode_presentation(&encode_presentation(&deck).unwrap()).unwrap();
        assert_eq!(restored, deck);
    }

    #[test]
    fn invalid_element_updates_are_atomic_and_type_checked() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let slide = deck.slides()[0].id();
        let text = deck.add_text(slide, rect(0.0, 0.0, 100.0, 40.0), "Title", TextStyle::default()).unwrap();
        let shape = deck.add_shape(slide, rect(0.0, 50.0, 100.0, 40.0), ShapeKind::Rectangle, None, None, 1.0).unwrap();
        let revision = deck.revision();

        assert!(deck.rename_slide(999, "Missing").is_err());
        assert!(deck.set_text_style(slide, shape, TextStyle::default()).is_err());
        assert!(deck.set_shape_style(slide, text, None, None, 1.0).is_err());
        assert!(deck.set_shape_style(slide, shape, Some(0x1000000), None, 1.0).is_err());
        assert!(deck.update_image(slide, text, "image.png", "not an image").is_err());
        assert!(deck.update_image(slide, shape, "", "empty").is_err());
        assert_eq!(deck.revision(), revision);
        assert_eq!(deck.slide(slide).unwrap().element(text).unwrap().kind(), &ElementKind::Text { text: "Title".to_owned(), style: TextStyle::default() });
    }

    #[test]
    fn invalid_mutations_do_not_change_revision_or_model() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let slide = deck.slides()[0].id();
        let revision = deck.revision();
        let invalid_style = TextStyle {
            font_size: f32::NAN,
            ..TextStyle::default()
        };
        assert!(
            deck.add_text(slide, rect(0.0, 0.0, 10.0, 10.0), "bad", invalid_style)
                .is_err()
        );
        assert_eq!(deck.revision(), revision);
        assert!(
            deck.add_shape(
                slide,
                rect(0.0, 0.0, 10.0, 10.0),
                ShapeKind::Rectangle,
                None,
                None,
                f32::NAN
            )
            .is_err()
        );
        assert_eq!(deck.revision(), revision);
        assert!(deck.remove_slide(slide).is_err());
        assert_eq!(deck.slides().len(), 1);
        assert_eq!(deck.revision(), revision);
    }

    #[test]
    fn binary_round_trip_preserves_all_slide_content() {
        let mut deck = Presentation::new("Roadmap", 1280.0, 720.0).unwrap();
        let slide = deck.slides()[0].id();
        deck.add_text(
            slide,
            rect(10.0, 20.0, 500.0, 100.0),
            "Heading",
            TextStyle::default(),
        )
        .unwrap();
        deck.add_shape(
            slide,
            rect(10.0, 140.0, 100.0, 60.0),
            ShapeKind::Arrow,
            Some(0x123456),
            None,
            0.0,
        )
        .unwrap();
        deck.add_image(
            slide,
            rect(200.0, 140.0, 200.0, 120.0),
            "media/chart.png",
            "Chart",
        )
        .unwrap();
        deck.set_slide_notes(slide, "Talk track").unwrap();
        let encoded = encode_presentation(&deck).unwrap();
        assert_eq!(decode_presentation(&encoded).unwrap(), deck);
    }

    #[test]
    fn rejects_corrupted_and_truncated_presentation_data() {
        let deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let mut encoded = encode_presentation(&deck).unwrap();
        encoded[12] ^= 0x80;
        assert_eq!(
            decode_presentation(&encoded).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let valid = encode_presentation(&deck).unwrap();
        assert!(decode_presentation(&valid[..valid.len() - 2]).is_err());
    }

    #[test]
    fn rejects_unknown_versions_and_trailing_bytes() {
        let deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let mut encoded = encode_presentation(&deck).unwrap();
        encoded[8] = 99;
        let checksum = crc32fast::hash(&encoded[..encoded.len() - 4]);
        let end = encoded.len();
        encoded[end - 4..].copy_from_slice(&checksum.to_le_bytes());
        assert!(decode_presentation(&encoded).is_err());
        let encoded = encode_presentation(&deck).unwrap();
        let end = encoded.len();
        let mut payload = encoded[..end - 4].to_vec();
        payload.push(0);
        let checksum = crc32fast::hash(&payload);
        payload.extend_from_slice(&checksum.to_le_bytes());
        assert!(decode_presentation(&payload).is_err());
    }

    #[test]
    fn validates_unique_ids_and_element_style() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let slide = deck.slides()[0].id();
        deck.add_shape(
            slide,
            rect(0.0, 0.0, 10.0, 10.0),
            ShapeKind::Rectangle,
            Some(0x1000000),
            None,
            1.0,
        )
        .unwrap_err();
        assert!(
            deck.add_image(slide, rect(0.0, 0.0, 10.0, 10.0), "", "missing")
                .is_err()
        );
        assert!(TextStyle::new("", 10.0, 0).is_err());
    }

    #[test]
    fn slide_names_and_notes_survive_serialization() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let slide = deck.add_slide("Overview").unwrap();
        deck.set_slide_notes(slide, "Explain the overview").unwrap();
        let restored = decode_presentation(&encode_presentation(&deck).unwrap()).unwrap();
        assert_eq!(restored.slide(slide).unwrap().name(), "Overview");
        assert_eq!(
            restored.slide(slide).unwrap().notes(),
            "Explain the overview"
        );
    }
}
