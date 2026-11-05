//! UI-independent presentation model, validation and versioned persistence.
//!
//! Coordinates are expressed in points. Image sources are references (paths or
//! URIs); PDF can embed host-supplied JPEG bytes through `PdfImageProvider` without
//! decoding them, while SVG keeps external references and PNG uses a host rasterizer.

use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::io::{self, Cursor, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use zip::ZipWriter;
use zip::write::FileOptions;

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
const MAX_PDF_IMAGE_BYTES: usize = 64 * 1024 * 1024;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlideLayout {
    Blank,
    Title,
    TitleAndContent,
    TwoColumns,
    SectionHeader,
    TitleOnly,
    Comparison,
    Quote,
    ContentWithSidebar,
}

/// A reusable set of presentation-wide colors and typography.
/// Applying a theme updates existing slide backgrounds and element styles.
#[derive(Debug, Clone, PartialEq)]
pub struct PresentationTheme {
    name: String,
    background: u32,
    title_font: String,
    body_font: String,
    title_color: u32,
    body_color: u32,
    accent_color: u32,
}
impl PresentationTheme {
    pub fn new(
        name: impl Into<String>,
        background: u32,
        title_font: impl Into<String>,
        body_font: impl Into<String>,
        title_color: u32,
        body_color: u32,
        accent_color: u32,
    ) -> io::Result<Self> {
        let theme = Self {
            name: name.into(),
            background,
            title_font: title_font.into(),
            body_font: body_font.into(),
            title_color,
            body_color,
            accent_color,
        };
        validate_string(&theme.name, "theme name")?;
        validate_string(&theme.title_font, "theme title font")?;
        validate_string(&theme.body_font, "theme body font")?;
        if theme.name.is_empty()
            || theme.title_font.is_empty()
            || theme.body_font.is_empty()
            || [
                theme.background,
                theme.title_color,
                theme.body_color,
                theme.accent_color,
            ]
            .iter()
            .any(|color| *color > 0x00ff_ffff)
        {
            return Err(invalid_input("presentation theme is invalid"));
        }
        Ok(theme)
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn background(&self) -> u32 {
        self.background
    }
    pub fn title_font(&self) -> &str {
        &self.title_font
    }
    pub fn body_font(&self) -> &str {
        &self.body_font
    }
    pub fn title_color(&self) -> u32 {
        self.title_color
    }
    pub fn body_color(&self) -> u32 {
        self.body_color
    }
    pub fn accent_color(&self) -> u32 {
        self.accent_color
    }
}

impl PresentationTheme {
    /// Built-in light theme for general-purpose decks.
    pub fn light() -> Self {
        Self::default()
    }

    /// Built-in dark theme with high-contrast text and a blue accent.
    pub fn dark() -> Self {
        Self::new(
            "Dark", 0x171a21, "Arial", "Arial", 0xf5f7fa, 0xd8dee9, 0x62a0ea,
        )
        .expect("built-in dark theme is valid")
    }

    /// Built-in business theme with restrained navy and teal colors.
    pub fn business() -> Self {
        Self::new(
            "Business", 0xffffff, "Arial", "Arial", 0x142b4a, 0x334155, 0x0f8b8d,
        )
        .expect("built-in business theme is valid")
    }

    /// Built-in high-contrast theme for improved legibility.
    pub fn high_contrast() -> Self {
        Self::new(
            "High contrast",
            0x000000,
            "Arial",
            "Arial",
            0xffffff,
            0xffffff,
            0xffff00,
        )
        .expect("built-in high-contrast theme is valid")
    }
}

impl Default for PresentationTheme {
    fn default() -> Self {
        Self::new(
            "Modern", 0xffffff, "Arial", "Arial", 0x202020, 0x404040, 0x356ae6,
        )
        .expect("built-in theme is valid")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementAlignment {
    Left,
    CenterHorizontal,
    Right,
    Top,
    CenterVertical,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistributionAxis {
    Horizontal,
    Vertical,
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

    /// Change the slide canvas dimensions in points (for example 960x540 for 16:9).
    pub fn set_dimensions(&mut self, width: f32, height: f32) -> io::Result<()> {
        validate_dimensions(width, height)?;
        if self.width == width && self.height == height {
            return Ok(());
        }
        self.bump_revision()?;
        self.width = width;
        self.height = height;
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
    /// Create a slide and populate it with editable text placeholders for a built-in layout.
    /// Placeholders are ordinary text elements so they can be styled, moved or replaced.
    pub fn add_slide_with_layout(
        &mut self,
        name: impl Into<String>,
        layout: SlideLayout,
    ) -> io::Result<u64> {
        let name = name.into();
        validate_string(&name, "slide name")?;
        let placeholder_count = match layout {
            SlideLayout::Blank => 0,
            SlideLayout::TitleOnly => 1,
            SlideLayout::Title
            | SlideLayout::TitleAndContent
            | SlideLayout::SectionHeader
            | SlideLayout::Quote => 2,
            SlideLayout::TwoColumns | SlideLayout::ContentWithSidebar => 3,
            SlideLayout::Comparison => 5,
        };
        if self.slides.len() >= MAX_SLIDES {
            return Err(invalid_input("slide limit reached"));
        }
        if self.next_id.checked_add(placeholder_count as u64).is_none() {
            return Err(invalid_input("presentation ID space exhausted"));
        }

        let margin_x = self.width * 0.07;
        let content_width = self.width - 2.0 * margin_x;
        let title_style = TextStyle::new("Arial", 30.0, 0x202020)?;
        let body_style = TextStyle::new("Arial", 20.0, 0x404040)?;
        let title_bounds = Rect::new(
            margin_x,
            self.height * 0.05,
            content_width,
            self.height * 0.15,
        )?;
        let body_y = self.height * 0.25;
        let body_height = self.height * 0.68;
        let slide_id = self.add_slide(name)?;

        match layout {
            SlideLayout::Blank => {}
            SlideLayout::Title => {
                self.add_text(
                    slide_id,
                    Rect::new(
                        margin_x,
                        self.height * 0.30,
                        content_width,
                        self.height * 0.16,
                    )?,
                    "Title",
                    title_style,
                )?;
                self.add_text(
                    slide_id,
                    Rect::new(
                        margin_x,
                        self.height * 0.52,
                        content_width,
                        self.height * 0.10,
                    )?,
                    "Subtitle",
                    body_style,
                )?;
            }
            SlideLayout::TitleAndContent => {
                self.add_text(slide_id, title_bounds, "Title", title_style)?;
                self.add_text(
                    slide_id,
                    Rect::new(margin_x, body_y, content_width, body_height)?,
                    "Click to add content",
                    body_style,
                )?;
            }
            SlideLayout::TwoColumns => {
                self.add_text(slide_id, title_bounds, "Title", title_style)?;
                let gap = self.width * 0.025;
                let column_width = (content_width - gap) / 2.0;
                self.add_text(
                    slide_id,
                    Rect::new(margin_x, body_y, column_width, body_height)?,
                    "Left content",
                    body_style.clone(),
                )?;
                self.add_text(
                    slide_id,
                    Rect::new(
                        margin_x + column_width + gap,
                        body_y,
                        column_width,
                        body_height,
                    )?,
                    "Right content",
                    body_style,
                )?;
            }
            SlideLayout::SectionHeader => {
                self.add_text(
                    slide_id,
                    Rect::new(
                        margin_x,
                        self.height * 0.34,
                        content_width,
                        self.height * 0.15,
                    )?,
                    "Section title",
                    title_style,
                )?;
                self.add_text(
                    slide_id,
                    Rect::new(
                        margin_x,
                        self.height * 0.52,
                        content_width,
                        self.height * 0.10,
                    )?,
                    "Section subtitle",
                    body_style,
                )?;
            }
            SlideLayout::TitleOnly => {
                self.add_text(slide_id, title_bounds, "Title", title_style)?;
            }
            SlideLayout::Comparison => {
                self.add_text(slide_id, title_bounds, "Comparison", title_style)?;
                let gap = self.width * 0.025;
                let column_width = (content_width - gap) / 2.0;
                let heading_height = self.height * 0.10;
                let column_y = body_y + heading_height;
                let column_height = (self.height - column_y - self.height * 0.07).max(0.01);
                self.add_text(
                    slide_id,
                    Rect::new(margin_x, body_y, column_width, heading_height)?,
                    "Option A",
                    body_style.clone(),
                )?;
                self.add_text(
                    slide_id,
                    Rect::new(
                        margin_x + column_width + gap,
                        body_y,
                        column_width,
                        heading_height,
                    )?,
                    "Option B",
                    body_style.clone(),
                )?;
                self.add_text(
                    slide_id,
                    Rect::new(margin_x, column_y, column_width, column_height)?,
                    "First option details",
                    body_style.clone(),
                )?;
                self.add_text(
                    slide_id,
                    Rect::new(
                        margin_x + column_width + gap,
                        column_y,
                        column_width,
                        column_height,
                    )?,
                    "Second option details",
                    body_style,
                )?;
            }
            SlideLayout::Quote => {
                let mut quote_style = title_style;
                quote_style.set_alignment(TextAlignment::Center);
                self.add_text(
                    slide_id,
                    Rect::new(
                        margin_x,
                        self.height * 0.25,
                        content_width,
                        self.height * 0.35,
                    )?,
                    "“Your quote here”",
                    quote_style,
                )?;
                let mut author_style = body_style;
                author_style.set_alignment(TextAlignment::Right);
                self.add_text(
                    slide_id,
                    Rect::new(
                        margin_x,
                        self.height * 0.66,
                        content_width,
                        self.height * 0.10,
                    )?,
                    "— Author",
                    author_style,
                )?;
            }
            SlideLayout::ContentWithSidebar => {
                self.add_text(slide_id, title_bounds, "Title", title_style)?;
                let gap = self.width * 0.025;
                let main_width = content_width * 0.68;
                let sidebar_width = content_width - main_width - gap;
                self.add_text(
                    slide_id,
                    Rect::new(margin_x, body_y, main_width, body_height)?,
                    "Main content",
                    body_style.clone(),
                )?;
                self.add_text(
                    slide_id,
                    Rect::new(
                        margin_x + main_width + gap,
                        body_y,
                        sidebar_width,
                        body_height,
                    )?,
                    "Sidebar",
                    body_style,
                )?;
            }
        }
        Ok(slide_id)
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
    /// Apply a theme to every slide in one atomic model revision.
    /// Text elements at 28 pt or larger are treated as titles; smaller text uses body styling.
    /// Existing emphasis, alignment, geometry, notes and media references are preserved.
    pub fn apply_theme(&mut self, theme: &PresentationTheme) -> io::Result<()> {
        // Revalidate so malformed values cannot enter through future deserialization paths.
        validate_string(&theme.name, "theme name")?;
        validate_string(&theme.title_font, "theme title font")?;
        validate_string(&theme.body_font, "theme body font")?;
        if theme.name.is_empty()
            || theme.title_font.is_empty()
            || theme.body_font.is_empty()
            || [
                theme.background,
                theme.title_color,
                theme.body_color,
                theme.accent_color,
            ]
            .iter()
            .any(|color| *color > 0x00ff_ffff)
        {
            return Err(invalid_input("presentation theme is invalid"));
        }
        let mut changed = false;
        for slide in &self.slides {
            if slide.background != theme.background {
                changed = true;
            }
            for element in &slide.elements {
                match &element.kind {
                    ElementKind::Text { style, .. } => {
                        let (font, color) = if style.font_size >= 28.0 {
                            (&theme.title_font, theme.title_color)
                        } else {
                            (&theme.body_font, theme.body_color)
                        };
                        if style.font_family != *font || style.color != color {
                            changed = true;
                        }
                    }
                    ElementKind::Shape {
                        fill: Some(fill), ..
                    } if *fill != theme.accent_color => changed = true,
                    _ => {}
                }
            }
        }
        if !changed {
            return Ok(());
        }
        self.bump_revision()?;
        for slide in &mut self.slides {
            slide.background = theme.background;
            for element in &mut slide.elements {
                match &mut element.kind {
                    ElementKind::Text { style, .. } => {
                        if style.font_size >= 28.0 {
                            style.font_family.clone_from(&theme.title_font);
                            style.color = theme.title_color;
                        } else {
                            style.font_family.clone_from(&theme.body_font);
                            style.color = theme.body_color;
                        }
                    }
                    ElementKind::Shape {
                        fill: Some(fill), ..
                    } => *fill = theme.accent_color,
                    _ => {}
                }
            }
        }
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

    /// Add a text box to a slide and return its stable element ID.
    pub fn add_text(
        &mut self,
        slide_id: u64,
        bounds: Rect,
        text: impl Into<String>,
        style: TextStyle,
    ) -> io::Result<u64> {
        bounds.validate()?;
        let text = text.into();
        validate_string(&text, "text element")?;
        style.validate()?;
        self.add_element(slide_id, bounds, ElementKind::Text { text, style })
    }

    /// Add a geometric shape to a slide and return its stable element ID.
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
        if fill.is_some_and(|color| color > 0x00ff_ffff)
            || stroke.is_some_and(|color| color > 0x00ff_ffff)
            || !stroke_width.is_finite()
            || !(0.0..=1000.0).contains(&stroke_width)
        {
            return Err(invalid_input("shape styling is invalid"));
        }
        self.add_element(
            slide_id,
            bounds,
            ElementKind::Shape {
                shape,
                fill,
                stroke,
                stroke_width,
            },
        )
    }

    /// Add an image reference to a slide. Image decoding and embedding are handled elsewhere.
    pub fn add_image(
        &mut self,
        slide_id: u64,
        bounds: Rect,
        source: impl Into<String>,
        alt_text: impl Into<String>,
    ) -> io::Result<u64> {
        let source = source.into();
        let alt_text = alt_text.into();
        bounds.validate()?;
        validate_string(&source, "image source")?;
        validate_string(&alt_text, "image alt text")?;
        if source.is_empty() {
            return Err(invalid_input("image source cannot be empty"));
        }
        self.add_element(slide_id, bounds, ElementKind::Image { source, alt_text })
    }

    fn add_element(&mut self, slide_id: u64, bounds: Rect, kind: ElementKind) -> io::Result<u64> {
        let slide = self
            .slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        if slide.elements.len() >= MAX_ELEMENTS_PER_SLIDE {
            return Err(invalid_input("slide element limit reached"));
        }
        self.bump_revision()?;
        let id = self.take_id()?;
        self.slide_mut(slide_id)?
            .elements
            .push(SlideElement { id, bounds, kind });
        Ok(id)
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
        let slide = self
            .slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        if !matches!(
            slide.element(element_id).map(|element| &element.kind),
            Some(ElementKind::Text { .. })
        ) {
            return Err(invalid_input("text element does not exist"));
        }
        self.bump_revision()?;
        let element = self
            .slide_mut(slide_id)?
            .elements
            .iter_mut()
            .find(|element| element.id == element_id)
            .unwrap();
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
        let slide = self
            .slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        if !matches!(
            slide.element(element_id).map(|element| &element.kind),
            Some(ElementKind::Shape { .. })
        ) {
            return Err(invalid_input("shape element does not exist"));
        }
        self.bump_revision()?;
        let element = self
            .slide_mut(slide_id)?
            .elements
            .iter_mut()
            .find(|element| element.id == element_id)
            .unwrap();
        if let ElementKind::Shape {
            fill: current_fill,
            stroke: current_stroke,
            stroke_width: current_width,
            ..
        } = &mut element.kind
        {
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
        let slide = self
            .slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        if !matches!(
            slide.element(element_id).map(|element| &element.kind),
            Some(ElementKind::Image { .. })
        ) {
            return Err(invalid_input("image element does not exist"));
        }
        self.bump_revision()?;
        let element = self
            .slide_mut(slide_id)?
            .elements
            .iter_mut()
            .find(|element| element.id == element_id)
            .unwrap();
        if let ElementKind::Image {
            source: current_source,
            alt_text: current_alt,
        } = &mut element.kind
        {
            *current_source = source;
            *current_alt = alt_text;
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
    /// Align at least two selected elements against their collective bounding box.
    /// Element sizes and stacking order are preserved.
    pub fn align_elements(
        &mut self,
        slide_id: u64,
        element_ids: &[u64],
        alignment: ElementAlignment,
    ) -> io::Result<()> {
        if element_ids.len() < 2 {
            return Err(invalid_input(
                "at least two elements are required for alignment",
            ));
        }
        let slide = self
            .slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        let mut selected = std::collections::HashSet::new();
        let mut indices = Vec::with_capacity(element_ids.len());
        for id in element_ids {
            if !selected.insert(*id) {
                return Err(invalid_input("element selection contains duplicate IDs"));
            }
            let index = slide
                .elements
                .iter()
                .position(|element| element.id == *id)
                .ok_or_else(|| invalid_input("element does not exist"))?;
            indices.push(index);
        }

        let min_x = indices
            .iter()
            .map(|index| slide.elements[*index].bounds.x)
            .fold(f32::INFINITY, f32::min);
        let min_y = indices
            .iter()
            .map(|index| slide.elements[*index].bounds.y)
            .fold(f32::INFINITY, f32::min);
        let max_x = indices
            .iter()
            .map(|index| {
                let bounds = slide.elements[*index].bounds;
                bounds.x + bounds.width
            })
            .fold(f32::NEG_INFINITY, f32::max);
        let max_y = indices
            .iter()
            .map(|index| {
                let bounds = slide.elements[*index].bounds;
                bounds.y + bounds.height
            })
            .fold(f32::NEG_INFINITY, f32::max);

        let mut updates = Vec::with_capacity(indices.len());
        for index in indices {
            let mut bounds = slide.elements[index].bounds;
            match alignment {
                ElementAlignment::Left => bounds.x = min_x,
                ElementAlignment::CenterHorizontal => {
                    bounds.x = (min_x + max_x - bounds.width) / 2.0;
                }
                ElementAlignment::Right => bounds.x = max_x - bounds.width,
                ElementAlignment::Top => bounds.y = min_y,
                ElementAlignment::CenterVertical => {
                    bounds.y = (min_y + max_y - bounds.height) / 2.0;
                }
                ElementAlignment::Bottom => bounds.y = max_y - bounds.height,
            }
            bounds.validate()?;
            if bounds != slide.elements[index].bounds {
                updates.push((index, bounds));
            }
        }
        if updates.is_empty() {
            return Ok(());
        }
        self.bump_revision()?;
        let elements = &mut self.slide_mut(slide_id)?.elements;
        for (index, bounds) in updates {
            elements[index].bounds = bounds;
        }
        Ok(())
    }

    /// Align one or more elements against the presentation canvas.
    /// Unlike group alignment, this also supports a single selected element.
    pub fn align_elements_to_canvas(
        &mut self,
        slide_id: u64,
        element_ids: &[u64],
        alignment: ElementAlignment,
    ) -> io::Result<()> {
        if element_ids.is_empty() {
            return Err(invalid_input(
                "at least one element is required for canvas alignment",
            ));
        }
        let slide = self
            .slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        let mut selected = std::collections::HashSet::new();
        let mut updates = Vec::with_capacity(element_ids.len());
        for id in element_ids {
            if !selected.insert(*id) {
                return Err(invalid_input("element selection contains duplicate IDs"));
            }
            let index = slide
                .elements
                .iter()
                .position(|element| element.id == *id)
                .ok_or_else(|| invalid_input("element does not exist"))?;
            let mut bounds = slide.elements[index].bounds;
            match alignment {
                ElementAlignment::Left => bounds.x = 0.0,
                ElementAlignment::CenterHorizontal => {
                    bounds.x = (self.width - bounds.width) / 2.0;
                }
                ElementAlignment::Right => bounds.x = self.width - bounds.width,
                ElementAlignment::Top => bounds.y = 0.0,
                ElementAlignment::CenterVertical => {
                    bounds.y = (self.height - bounds.height) / 2.0;
                }
                ElementAlignment::Bottom => bounds.y = self.height - bounds.height,
            }
            bounds.validate()?;
            if bounds != slide.elements[index].bounds {
                updates.push((index, bounds));
            }
        }
        if updates.is_empty() {
            return Ok(());
        }
        self.bump_revision()?;
        let elements = &mut self.slide_mut(slide_id)?.elements;
        for (index, bounds) in updates {
            elements[index].bounds = bounds;
        }
        Ok(())
    }

    /// Distribute three or more elements evenly between the first and last positions.
    /// The input order is ignored; elements are sorted by their current axis position.
    pub fn distribute_elements(
        &mut self,
        slide_id: u64,
        element_ids: &[u64],
        axis: DistributionAxis,
    ) -> io::Result<()> {
        if element_ids.len() < 3 {
            return Err(invalid_input(
                "at least three elements are required for distribution",
            ));
        }
        let slide = self
            .slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        let mut selected = std::collections::HashSet::new();
        let mut indices = Vec::with_capacity(element_ids.len());
        for id in element_ids {
            if !selected.insert(*id) {
                return Err(invalid_input("element selection contains duplicate IDs"));
            }
            let index = slide
                .elements
                .iter()
                .position(|element| element.id == *id)
                .ok_or_else(|| invalid_input("element does not exist"))?;
            indices.push(index);
        }

        indices.sort_by(|left, right| {
            let a = slide.elements[*left].bounds;
            let b = slide.elements[*right].bounds;
            let (a_position, b_position) = match axis {
                DistributionAxis::Horizontal => (a.x, b.x),
                DistributionAxis::Vertical => (a.y, b.y),
            };
            a_position.total_cmp(&b_position)
        });

        let extent_start = match axis {
            DistributionAxis::Horizontal => slide.elements[indices[0]].bounds.x,
            DistributionAxis::Vertical => slide.elements[indices[0]].bounds.y,
        };
        let last = slide.elements[*indices.last().unwrap()].bounds;
        let extent_end = match axis {
            DistributionAxis::Horizontal => last.x + last.width,
            DistributionAxis::Vertical => last.y + last.height,
        };
        let total_size: f32 = indices
            .iter()
            .map(|index| {
                let bounds = slide.elements[*index].bounds;
                match axis {
                    DistributionAxis::Horizontal => bounds.width,
                    DistributionAxis::Vertical => bounds.height,
                }
            })
            .sum();
        let gap = (extent_end - extent_start - total_size) / (indices.len() - 1) as f32;
        if !gap.is_finite() || gap < 0.0 {
            return Err(invalid_input(
                "selected elements do not fit in the distribution range",
            ));
        }

        let mut cursor = extent_start;
        let mut updates = Vec::with_capacity(indices.len());
        for index in indices {
            let mut bounds = slide.elements[index].bounds;
            match axis {
                DistributionAxis::Horizontal => {
                    bounds.x = cursor;
                    cursor += bounds.width + gap;
                }
                DistributionAxis::Vertical => {
                    bounds.y = cursor;
                    cursor += bounds.height + gap;
                }
            }
            bounds.validate()?;
            if bounds != slide.elements[index].bounds {
                updates.push((index, bounds));
            }
        }
        if updates.is_empty() {
            return Ok(());
        }
        self.bump_revision()?;
        let elements = &mut self.slide_mut(slide_id)?.elements;
        for (index, bounds) in updates {
            elements[index].bounds = bounds;
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
    /// Duplicate an element directly after its source, assigning a fresh stable ID.
    pub fn duplicate_element(&mut self, slide_id: u64, element_id: u64) -> io::Result<u64> {
        let slide = self
            .slide(slide_id)
            .ok_or_else(|| invalid_input("slide does not exist"))?;
        if slide.elements.len() >= MAX_ELEMENTS_PER_SLIDE {
            return Err(invalid_input("slide element limit reached"));
        }
        let mut duplicate = slide
            .element(element_id)
            .cloned()
            .ok_or_else(|| invalid_input("element does not exist"))?;
        self.bump_revision()?;
        duplicate.id = self.take_id()?;
        let id = duplicate.id;
        let elements = &mut self.slide_mut(slide_id)?.elements;
        let index = elements
            .iter()
            .position(|element| element.id == element_id)
            .ok_or_else(|| invalid_input("element does not exist"))?;
        elements.insert(index + 1, duplicate);
        Ok(id)
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

/// Render a slide to SVG. Image references remain external and are not embedded.
pub fn render_slide_svg(presentation: &Presentation, slide_id: u64) -> io::Result<String> {
    presentation.validate()?;
    let slide = presentation
        .slide(slide_id)
        .ok_or_else(|| invalid_input("slide does not exist"))?;
    let mut svg = String::with_capacity(4096);
    writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="{}pt" height="{}pt" viewBox="0 0 {} {}" role="img" aria-label="{}">"#,
        presentation.width,
        presentation.height,
        presentation.width,
        presentation.height,
        escape_xml(&slide.name)
    )
    .expect("writing to String cannot fail");
    writeln!(svg, "<title>{}</title>", escape_xml(&slide.name))
        .expect("writing to String cannot fail");
    writeln!(svg, "<desc>{}</desc>", escape_xml(presentation.title()))
        .expect("writing to String cannot fail");
    svg.push_str("<defs><marker id=\"arrowhead\" markerWidth=\"10\" markerHeight=\"7\" refX=\"9\" refY=\"3.5\" orient=\"auto\"><polygon points=\"0 0, 10 3.5, 0 7\" fill=\"context-stroke\"/></marker></defs>\n");
    writeln!(
        svg,
        "<rect x=\"0\" y=\"0\" width=\"100%\" height=\"100%\" fill=\"#{:06x}\"/>",
        slide.background
    )
    .expect("writing to String cannot fail");

    for element in &slide.elements {
        let bounds = element.bounds;
        match &element.kind {
            ElementKind::Text { text, style } => {
                let (anchor, x) = match style.alignment {
                    TextAlignment::Left | TextAlignment::Justify => ("start", bounds.x),
                    TextAlignment::Center => ("middle", bounds.x + bounds.width / 2.0),
                    TextAlignment::Right => ("end", bounds.x + bounds.width),
                };
                let decoration = if style.underline { "underline" } else { "none" };
                let weight = if style.bold { "bold" } else { "normal" };
                let italic = if style.italic { "italic" } else { "normal" };
                writeln!(
                    svg,
                    "<defs><clipPath id=\"clip-{}\"><rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/></clipPath></defs>",
                    element.id, bounds.x, bounds.y, bounds.width, bounds.height
                )
                .expect("writing to String cannot fail");
                writeln!(
                    svg,
                    "<text id=\"element-{}\" x=\"{}\" y=\"{}\" fill=\"#{:06x}\" font-family=\"{}\" font-size=\"{}\" font-weight=\"{}\" font-style=\"{}\" text-anchor=\"{}\" text-decoration=\"{}\" clip-path=\"url(#clip-{})\">",
                    element.id, x, bounds.y + style.font_size, style.color,
                    escape_xml(&style.font_family), style.font_size, weight, italic,
                    anchor, decoration, element.id
                )
                .expect("writing to String cannot fail");
                let lines = wrap_text_for_svg(text, bounds.width, style.font_size);
                for (index, line) in lines.iter().enumerate() {
                    if index == 0 {
                        writeln!(svg, "<tspan x=\"{}\">{}</tspan>", x, escape_xml(line))
                            .expect("writing to String cannot fail");
                    } else {
                        writeln!(
                            svg,
                            "<tspan x=\"{}\" dy=\"{}\">{}</tspan>",
                            x,
                            style.font_size * 1.2,
                            escape_xml(line)
                        )
                        .expect("writing to String cannot fail");
                    }
                }
                svg.push_str("</text>\n");
            }
            ElementKind::Shape {
                shape,
                fill,
                stroke,
                stroke_width,
            } => {
                let fill = fill.map_or_else(|| "none".to_owned(), |color| format!("#{color:06x}"));
                let stroke =
                    stroke.map_or_else(|| "none".to_owned(), |color| format!("#{color:06x}"));
                match shape {
                    ShapeKind::Rectangle => writeln!(
                        svg,
                        "<rect id=\"element-{}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"{}\"/>",
                        element.id, bounds.x, bounds.y, bounds.width, bounds.height, fill, stroke, stroke_width
                    ),
                    ShapeKind::Ellipse => writeln!(
                        svg,
                        "<ellipse id=\"element-{}\" cx=\"{}\" cy=\"{}\" rx=\"{}\" ry=\"{}\" fill=\"{}\" stroke=\"{}\" stroke-width=\"{}\"/>",
                        element.id, bounds.x + bounds.width / 2.0, bounds.y + bounds.height / 2.0,
                        bounds.width / 2.0, bounds.height / 2.0, fill, stroke, stroke_width
                    ),
                    ShapeKind::Line | ShapeKind::Arrow => {
                        let stroke = if stroke == "none" { "#000000" } else { &stroke };
                        let marker = if *shape == ShapeKind::Arrow {
                            " marker-end=\"url(#arrowhead)\""
                        } else {
                            ""
                        };
                        writeln!(
                            svg,
                            "<line id=\"element-{}\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"{}\" stroke-width=\"{}\"{}/>",
                            element.id, bounds.x, bounds.y, bounds.x + bounds.width,
                            bounds.y + bounds.height, stroke, (*stroke_width).max(1.0), marker
                        )
                    }
                }
                .expect("writing to String cannot fail");
            }
            ElementKind::Image { source, alt_text } => {
                writeln!(
                    svg,
                    "<image id=\"element-{}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" href=\"{}\" xlink:href=\"{}\" preserveAspectRatio=\"xMidYMid meet\"><title>{}</title></image>",
                    element.id, bounds.x, bounds.y, bounds.width, bounds.height,
                    escape_xml(source), escape_xml(source), escape_xml(alt_text)
                )
                .expect("writing to String cannot fail");
            }
        }
    }
    svg.push_str("</svg>\n");
    Ok(svg)
}

/// Render every slide in presentation order, preserving each stable slide ID.
pub fn render_all_slides_svg(presentation: &Presentation) -> io::Result<Vec<(u64, String)>> {
    presentation.validate()?;
    presentation
        .slides()
        .iter()
        .map(|slide| {
            let id = slide.id();
            render_slide_svg(presentation, id).map(|svg| (id, svg))
        })
        .collect()
}

/// Adapter implemented by the application's graphics engine (or another PNG renderer).
///
/// The presentation engine deliberately does not rasterize SVG itself. Implementors
/// receive the SVG produced by this crate and must return a PNG at the requested size.
pub trait PngRasterizer {
    fn rasterize_svg(&self, svg: &str, width: u32, height: u32) -> io::Result<Vec<u8>>;
}

/// Export one slide as PNG through an externally supplied rasterizer.
///
/// This is an integration boundary, not a built-in graphics or rasterization engine.
pub fn render_slide_png_with<R: PngRasterizer>(
    presentation: &Presentation,
    slide_id: u64,
    width: u32,
    height: u32,
    rasterizer: &R,
) -> io::Result<Vec<u8>> {
    validate_png_dimensions(width, height)?;
    let svg = render_slide_svg(presentation, slide_id)?;
    let png = rasterizer.rasterize_svg(&svg, width, height)?;
    validate_png(&png, width, height)?;
    Ok(png)
}

/// Export every slide as PNG through an externally supplied rasterizer.
///
/// Results preserve presentation order and include each slide's stable ID.
pub fn render_presentation_png_with<R: PngRasterizer>(
    presentation: &Presentation,
    width: u32,
    height: u32,
    rasterizer: &R,
) -> io::Result<Vec<(u64, Vec<u8>)>> {
    validate_png_dimensions(width, height)?;
    render_all_slides_svg(presentation)?
        .into_iter()
        .map(|(slide_id, svg)| {
            let png = rasterizer.rasterize_svg(&svg, width, height)?;
            validate_png(&png, width, height)?;
            Ok((slide_id, png))
        })
        .collect()
}

fn validate_png_dimensions(width: u32, height: u32) -> io::Result<()> {
    if width == 0 || height == 0 {
        return Err(invalid_input("PNG dimensions must be greater than zero"));
    }
    Ok(())
}

/// Validate PNG signature, chunk framing/CRCs, IHDR dimensions, image data and IEND.
fn validate_png(png: &[u8], expected_width: u32, expected_height: u32) -> io::Result<()> {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if !png.starts_with(SIGNATURE) {
        return Err(invalid_input("rasterizer did not return a PNG file"));
    }

    let mut offset = SIGNATURE.len();
    let mut chunk_index = 0usize;
    let mut has_palette = false;
    let mut color_type = None;
    let mut has_image_data = false;
    let mut image_data_ended = false;
    let mut has_end = false;
    while offset < png.len() {
        if png.len() - offset < 12 {
            return Err(invalid_input("truncated PNG chunk"));
        }
        let length = u32::from_be_bytes(
            png[offset..offset + 4]
                .try_into()
                .expect("four-byte PNG chunk length"),
        ) as usize;
        let chunk_end = offset
            .checked_add(12)
            .and_then(|value| value.checked_add(length))
            .filter(|end| *end <= png.len())
            .ok_or_else(|| invalid_input("PNG chunk exceeds file bounds"))?;
        if length > 0x7fff_ffff {
            return Err(invalid_input("PNG chunk exceeds the format size limit"));
        }
        let chunk_type = &png[offset + 4..offset + 8];
        if !chunk_type.iter().all(u8::is_ascii_alphabetic) || chunk_type[2].is_ascii_lowercase() {
            return Err(invalid_input("PNG chunk type is invalid"));
        }
        let data_start = offset + 8;
        let data_end = data_start + length;
        let expected_crc = u32::from_be_bytes(
            png[data_end..data_end + 4]
                .try_into()
                .expect("four-byte PNG chunk CRC"),
        );
        let actual_crc = crc32fast::hash(&png[offset + 4..data_end]);
        if expected_crc != actual_crc {
            return Err(invalid_input("PNG chunk CRC mismatch"));
        }

        if chunk_index == 0 {
            if chunk_type != b"IHDR" || length != 13 {
                return Err(invalid_input("PNG must start with a 13-byte IHDR chunk"));
            }
            let actual_width = u32::from_be_bytes(
                png[data_start..data_start + 4]
                    .try_into()
                    .expect("four-byte PNG width"),
            );
            let actual_height = u32::from_be_bytes(
                png[data_start + 4..data_start + 8]
                    .try_into()
                    .expect("four-byte PNG height"),
            );
            let bit_depth = png[data_start + 8];
            let current_color_type = png[data_start + 9];
            let compression = png[data_start + 10];
            let filter = png[data_start + 11];
            let interlace = png[data_start + 12];
            let valid_bit_depth = match current_color_type {
                0 => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
                2 => matches!(bit_depth, 8 | 16),
                3 => matches!(bit_depth, 1 | 2 | 4 | 8),
                4 | 6 => matches!(bit_depth, 8 | 16),
                _ => false,
            };
            if actual_width != expected_width || actual_height != expected_height {
                return Err(invalid_input(
                    "rasterizer returned unexpected PNG dimensions",
                ));
            }
            if actual_width == 0 || actual_height == 0 {
                return Err(invalid_input("PNG dimensions must be greater than zero"));
            }
            if !valid_bit_depth || compression != 0 || filter != 0 || interlace > 1 {
                return Err(invalid_input(
                    "PNG IHDR contains unsupported or invalid values",
                ));
            }
            color_type = Some(current_color_type);
        } else if chunk_type == b"IHDR" {
            return Err(invalid_input("PNG contains more than one IHDR chunk"));
        }

        match chunk_type {
            b"PLTE" => {
                if has_palette
                    || has_image_data
                    || !matches!(color_type, Some(2 | 3 | 6))
                    || length == 0
                    || length > 768
                    || length % 3 != 0
                {
                    return Err(invalid_input("PNG palette chunk is invalid or misplaced"));
                }
                has_palette = true;
            }
            b"IDAT" => {
                if color_type == Some(3) && !has_palette {
                    return Err(invalid_input("indexed PNG is missing its palette"));
                }
                if image_data_ended {
                    return Err(invalid_input("PNG IDAT chunks must be consecutive"));
                }
                has_image_data = true;
            }
            b"IEND" => {
                if length != 0 || !has_image_data || chunk_end != png.len() {
                    return Err(invalid_input("invalid PNG IEND placement or image data"));
                }
                has_end = true;
                break;
            }
            _ if has_image_data => image_data_ended = true,
            _ => {}
        }

        chunk_index += 1;
        offset = chunk_end;
    }

    if !has_end {
        return Err(invalid_input("PNG is missing its IEND chunk"));
    }
    Ok(())
}

/// Export one slide as a standalone PDF document.
pub fn render_slide_pdf(presentation: &Presentation, slide_id: u64) -> io::Result<Vec<u8>> {
    presentation.validate()?;
    let slide = presentation
        .slide(slide_id)
        .ok_or_else(|| invalid_input("slide does not exist"))?;
    render_pdf_document(presentation, std::slice::from_ref(slide))
}

/// A JPEG image supplied by the host application for direct PDF embedding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfJpegImage {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub color_space: PdfImageColorSpace,
}

/// Color spaces supported by direct JPEG embedding in PDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfImageColorSpace {
    Gray,
    Rgb,
    Cmyk,
}

impl PdfJpegImage {
    /// Validate the JPEG frame header and ensure its dimensions/color space match the metadata.
    pub fn validate(&self) -> io::Result<()> {
        if self.width == 0
            || self.height == 0
            || self.bytes.len() < 4
            || self.bytes.len() > MAX_PDF_IMAGE_BYTES
            || self.bytes[0..2] != [0xff, 0xd8]
            || self.bytes[self.bytes.len() - 2..] != [0xff, 0xd9]
        {
            return Err(invalid_input("invalid JPEG image data"));
        }
        let (width, height, components) = jpeg_frame_info(&self.bytes)?;
        let expected_components = match self.color_space {
            PdfImageColorSpace::Gray => 1,
            PdfImageColorSpace::Rgb => 3,
            PdfImageColorSpace::Cmyk => 4,
        };
        if width != self.width || height != self.height || components != expected_components {
            return Err(invalid_input(
                "JPEG metadata does not match its frame header",
            ));
        }
        Ok(())
    }
}

/// Resolves image references to already encoded JPEG data.
/// The application owns file access, decoding and image preparation.
pub trait PdfImageProvider {
    fn resolve_jpeg(&self, source: &str) -> io::Result<Option<PdfJpegImage>>;
}

/// Export one slide as PDF, embedding JPEGs returned by the supplied provider.
pub fn render_slide_pdf_with_images(
    presentation: &Presentation,
    slide_id: u64,
    provider: &dyn PdfImageProvider,
) -> io::Result<Vec<u8>> {
    presentation.validate()?;
    let slide = presentation
        .slide(slide_id)
        .ok_or_else(|| invalid_input("slide does not exist"))?;
    render_pdf_document_with_images(presentation, std::slice::from_ref(slide), Some(provider))
}

/// Export a complete presentation as PDF, embedding JPEGs returned by the supplied provider.
pub fn render_presentation_pdf_with_images(
    presentation: &Presentation,
    provider: &dyn PdfImageProvider,
) -> io::Result<Vec<u8>> {
    presentation.validate()?;
    render_pdf_document_with_images(presentation, &presentation.slides, Some(provider))
}

/// Export the complete presentation as a multi-page PDF document.
pub fn render_presentation_pdf(presentation: &Presentation) -> io::Result<Vec<u8>> {
    presentation.validate()?;
    render_pdf_document(presentation, &presentation.slides)
}

fn render_pdf_document(presentation: &Presentation, slides: &[Slide]) -> io::Result<Vec<u8>> {
    render_pdf_document_with_images(presentation, slides, None)
}

fn render_pdf_document_with_images(
    presentation: &Presentation,
    slides: &[Slide],
    provider: Option<&dyn PdfImageProvider>,
) -> io::Result<Vec<u8>> {
    if slides.is_empty() {
        return Err(invalid_input("a PDF must contain at least one page"));
    }
    // Twelve built-in PDF Type 1 fonts: Helvetica, Times and Courier,
    // each with regular, bold, italic and bold-italic variants.
    const FONT_COUNT: usize = 12;
    const FIRST_PAGE_ID: usize = 3 + FONT_COUNT;
    let mut objects: Vec<Vec<u8>> = vec![Vec::new(); FIRST_PAGE_ID + slides.len() * 2];
    objects[1] = b"<< /Type /Catalog /Pages 2 0 R >>".to_vec();

    let page_ids = (0..slides.len())
        .map(|index| FIRST_PAGE_ID + index * 2)
        .collect::<Vec<_>>();
    let kids = page_ids
        .iter()
        .map(|id| format!("{id} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    objects[2] = format!(
        "<< /Type /Pages /Count {} /Kids [{}] >>",
        slides.len(),
        kids
    )
    .into_bytes();

    let font_names = [
        "Helvetica",
        "Helvetica-Bold",
        "Helvetica-Oblique",
        "Helvetica-BoldOblique",
        "Times-Roman",
        "Times-Bold",
        "Times-Italic",
        "Times-BoldItalic",
        "Courier",
        "Courier-Bold",
        "Courier-Oblique",
        "Courier-BoldOblique",
    ];
    for (offset, base_font) in font_names.iter().enumerate() {
        let object_id = 3 + offset;
        objects[object_id] = format!(
            "<< /Type /Font /Subtype /Type1 /BaseFont /{base_font} /Encoding /WinAnsiEncoding >>"
        )
        .into_bytes();
    }

    let font_resources = (0..FONT_COUNT)
        .map(|offset| format!("/F{} {} 0 R", offset + 1, offset + 3))
        .collect::<Vec<_>>()
        .join(" ");

    // Resolve each referenced image once, then share its PDF XObject across slides.
    let mut images: Vec<(String, PdfJpegImage)> = Vec::new();
    if let Some(provider) = provider {
        for slide in slides {
            for element in &slide.elements {
                if let ElementKind::Image { source, .. } = &element.kind {
                    if images.iter().any(|(existing, _)| existing == source) {
                        continue;
                    }
                    if let Some(image) = provider.resolve_jpeg(source)? {
                        image.validate()?;
                        images.push((source.clone(), image));
                    }
                }
            }
        }
    }
    let image_object_start = FIRST_PAGE_ID + slides.len() * 2;
    let image_ids = images
        .iter()
        .enumerate()
        .map(|(index, (source, _))| (source.clone(), image_object_start + index))
        .collect::<HashMap<_, _>>();

    for (index, slide) in slides.iter().enumerate() {
        let page_id = page_ids[index];
        let content_id = page_id + 1;
        let image_names = image_ids
            .iter()
            .map(|(source, id)| (source.clone(), format!("Im{}", id - image_object_start + 1)))
            .collect::<HashMap<_, _>>();
        let stream = render_pdf_page_content(presentation, slide, &image_names)?;
        let image_resources = slide
            .elements
            .iter()
            .filter_map(|element| {
                if let ElementKind::Image { source, .. } = &element.kind {
                    let name = image_names.get(source)?;
                    let id = image_ids.get(source)?;
                    Some(format!("/{name} {id} 0 R"))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let xobject_resources = if image_resources.is_empty() {
            String::new()
        } else {
            format!(" /XObject << {} >>", image_resources.join(" "))
        };
        objects[page_id] = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {} {}] /Resources << /Font << {font_resources} >>{xobject_resources} >> /Contents {content_id} 0 R >>",
            presentation.width, presentation.height
        )
        .into_bytes();
        let mut stream_object = format!("<< /Length {} >>\nstream\n", stream.len()).into_bytes();
        stream_object.extend_from_slice(stream.as_bytes());
        stream_object.extend_from_slice(b"\nendstream");
        objects[content_id] = stream_object;
    }

    objects.resize(image_object_start + images.len(), Vec::new());
    for (index, (_, image)) in images.iter().enumerate() {
        let object_id = image_object_start + index;
        let color_space = match image.color_space {
            PdfImageColorSpace::Gray => "/DeviceGray",
            PdfImageColorSpace::Rgb => "/DeviceRGB",
            PdfImageColorSpace::Cmyk => "/DeviceCMYK",
        };
        // CMYK JPEG samples are conventionally inverted when consumed by PDF.
        let decode = if image.color_space == PdfImageColorSpace::Cmyk {
            " /Decode [1 0 1 0 1 0 1 0]"
        } else {
            ""
        };
        let mut stream_object = format!(
            "<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace {color_space} /BitsPerComponent 8 /Filter /DCTDecode /Length {}{decode} >>\nstream\n",
            image.width, image.height, image.bytes.len()
        ).into_bytes();
        stream_object.extend_from_slice(&image.bytes);
        stream_object.extend_from_slice(b"\nendstream");
        objects[object_id] = stream_object;
    }

    let mut pdf = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = vec![0usize; objects.len()];
    for id in 1..objects.len() {
        offsets[id] = pdf.len();
        pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        pdf.extend_from_slice(&objects[id]);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref_offset = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len()).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R /Info << /Title <{}> >> >>\nstartxref\n{}\n%%EOF\n",
            objects.len(),
            pdf_hex_string(presentation.title()),
            xref_offset
        )
        .as_bytes(),
    );
    Ok(pdf)
}

/// Read JPEG SOF metadata without decoding or rasterizing the image.
fn jpeg_frame_info(bytes: &[u8]) -> io::Result<(u32, u32, u8)> {
    if bytes.len() < 4 || bytes[0..2] != [0xff, 0xd8] {
        return Err(invalid_input("invalid JPEG start marker"));
    }
    let mut offset = 2usize;
    while offset + 4 <= bytes.len() {
        if bytes[offset] != 0xff {
            offset += 1;
            continue;
        }
        while offset < bytes.len() && bytes[offset] == 0xff {
            offset += 1;
        }
        if offset >= bytes.len() {
            break;
        }
        let marker = bytes[offset];
        offset += 1;
        if marker == 0xd9 || marker == 0xda {
            break;
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if offset + 2 > bytes.len() {
            break;
        }
        let segment_length = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
        if segment_length < 2 || offset + segment_length > bytes.len() {
            return Err(invalid_input("invalid JPEG segment length"));
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            if segment_length < 8 {
                return Err(invalid_input("invalid JPEG frame header"));
            }
            let height = u16::from_be_bytes([bytes[offset + 3], bytes[offset + 4]]) as u32;
            let width = u16::from_be_bytes([bytes[offset + 5], bytes[offset + 6]]) as u32;
            let components = bytes[offset + 7];
            if width == 0 || height == 0 || !matches!(components, 1 | 3 | 4) {
                return Err(invalid_input(
                    "unsupported JPEG frame dimensions or components",
                ));
            }
            return Ok((width, height, components));
        }
        offset += segment_length;
    }
    Err(invalid_input("JPEG frame header not found"))
}

/// Map common font-family names to the portable PDF base-14 font families.
/// Fonts outside these families fall back to Helvetica; arbitrary fonts are not embedded.
fn pdf_font_resource(family: &str, bold: bool, italic: bool) -> &'static str {
    let normalized = family
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    let family_offset = if normalized.contains("courier")
        || normalized.contains("consolas")
        || normalized.contains("monospace")
    {
        8
    } else if normalized.contains("times")
        || normalized.contains("georgia")
        || normalized.contains("cambria")
        || normalized.contains("palatino")
        || (normalized.contains("serif") && !normalized.contains("sansserif"))
    {
        4
    } else {
        0
    };
    let style_offset = match (bold, italic) {
        (false, false) => 0,
        (true, false) => 1,
        (false, true) => 2,
        (true, true) => 3,
    };
    match family_offset + style_offset {
        0 => "F1",
        1 => "F2",
        2 => "F3",
        3 => "F4",
        4 => "F5",
        5 => "F6",
        6 => "F7",
        7 => "F8",
        8 => "F9",
        9 => "F10",
        10 => "F11",
        _ => "F12",
    }
}

fn render_pdf_page_content(
    presentation: &Presentation,
    slide: &Slide,
    image_names: &HashMap<String, String>,
) -> io::Result<String> {
    let mut output = String::with_capacity(4096);
    let page_height = presentation.height;
    let background = pdf_rgb(slide.background);
    writeln!(
        output,
        "q {background} rg 0 0 {} {} re f Q",
        presentation.width, presentation.height
    )
    .expect("writing to String cannot fail");

    for element in &slide.elements {
        let bounds = element.bounds;
        let bottom = page_height - bounds.y - bounds.height;
        match &element.kind {
            ElementKind::Text { text, style } => {
                let font = pdf_font_resource(&style.font_family, style.bold, style.italic);
                let color = pdf_rgb(style.color);
                let lines = wrap_text_for_svg(text, bounds.width, style.font_size);
                let line_height = style.font_size * 1.2;
                let clip_bottom = page_height - bounds.y - bounds.height;
                writeln!(
                    output,
                    "q {} {} {} {} re W n",
                    bounds.x, clip_bottom, bounds.width, bounds.height
                )
                .expect("writing to String cannot fail");
                for (line_index, line) in lines.iter().enumerate() {
                    let estimated_width = line.chars().count() as f32 * style.font_size * 0.5;
                    let x = match style.alignment {
                        TextAlignment::Left | TextAlignment::Justify => bounds.x,
                        TextAlignment::Center => {
                            bounds.x + ((bounds.width - estimated_width) / 2.0).max(0.0)
                        }
                        TextAlignment::Right => {
                            bounds.x + (bounds.width - estimated_width).max(0.0)
                        }
                    };
                    let baseline =
                        page_height - bounds.y - style.font_size - line_index as f32 * line_height;
                    writeln!(
                        output,
                        "BT /{font} {} Tf {color} rg 1 0 0 1 {} {} Tm <{}> Tj ET",
                        style.font_size,
                        x,
                        baseline,
                        pdf_hex_string(line)
                    )
                    .expect("writing to String cannot fail");
                    if style.underline {
                        let underline_y = baseline - style.font_size * 0.12;
                        let underline_width = estimated_width.min(bounds.width);
                        writeln!(
                            output,
                            "q {color} RG 0.6 w {} {} m {} {} l S Q",
                            x,
                            underline_y,
                            x + underline_width,
                            underline_y
                        )
                        .expect("writing to String cannot fail");
                    }
                }
                output.push_str("Q\n");
            }
            ElementKind::Shape {
                shape,
                fill,
                stroke,
                stroke_width,
            } => {
                let fill_command = fill
                    .map(|color| format!("{} rg ", pdf_rgb(color)))
                    .unwrap_or_default();
                let stroke_command = stroke
                    .map(|color| format!("{} RG ", pdf_rgb(color)))
                    .unwrap_or_default();
                let paint = match (fill.is_some(), stroke.is_some()) {
                    (true, true) => "B",
                    (true, false) => "f",
                    (false, true) => "S",
                    (false, false) => "n",
                };
                match shape {
                    ShapeKind::Rectangle => {
                        writeln!(
                            output,
                            "q {fill_command}{stroke_command}{} w {} {} {} {} re {paint} Q",
                            stroke_width, bounds.x, bottom, bounds.width, bounds.height
                        )
                        .expect("writing to String cannot fail");
                    }
                    ShapeKind::Ellipse => {
                        let k = 0.552_284_8_f32;
                        let cx = bounds.x + bounds.width / 2.0;
                        let cy = bottom + bounds.height / 2.0;
                        let rx = bounds.width / 2.0;
                        let ry = bounds.height / 2.0;
                        writeln!(
                            output,
                            "q {fill_command}{stroke_command}{} w {} {} m {} {} {} {} {} {} c {} {} {} {} {} {} c {} {} {} {} {} {} c {} {} {} {} {} {} c {paint} Q",
                            stroke_width,
                            cx + rx, cy,
                            cx + rx, cy + k * ry, cx + k * rx, cy + ry, cx, cy + ry,
                            cx - k * rx, cy + ry, cx - rx, cy + k * ry, cx - rx, cy,
                            cx - rx, cy - k * ry, cx - k * rx, cy - ry, cx, cy - ry,
                            cx + k * rx, cy - ry, cx + rx, cy - k * ry, cx + rx, cy
                        )
                        .expect("writing to String cannot fail");
                    }
                    ShapeKind::Line | ShapeKind::Arrow => {
                        let color = stroke.unwrap_or(0x000000);
                        let x1 = bounds.x;
                        let y1 = page_height - bounds.y;
                        let x2 = bounds.x + bounds.width;
                        let y2 = page_height - bounds.y - bounds.height;
                        writeln!(
                            output,
                            "q {} RG {} w {} {} m {} {} l S",
                            pdf_rgb(color),
                            stroke_width.max(1.0),
                            x1,
                            y1,
                            x2,
                            y2
                        )
                        .expect("writing to String cannot fail");
                        if *shape == ShapeKind::Arrow {
                            let dx = x2 - x1;
                            let dy = y2 - y1;
                            let length = (dx * dx + dy * dy).sqrt().max(0.001);
                            let ux = dx / length;
                            let uy = dy / length;
                            let head = (stroke_width.max(1.0) * 4.0).max(8.0);
                            let half = head * 0.45;
                            let bx = x2 - ux * head;
                            let by = y2 - uy * head;
                            let px = -uy * half;
                            let py = ux * half;
                            writeln!(
                                output,
                                "{} rg {} {} m {} {} l {} {} l h f Q",
                                pdf_rgb(color),
                                x2,
                                y2,
                                bx + px,
                                by + py,
                                bx - px,
                                by - py
                            )
                            .expect("writing to String cannot fail");
                        } else {
                            output.push_str("Q\n");
                        }
                    }
                }
            }
            ElementKind::Image { source, alt_text } => {
                if let Some(name) = image_names.get(source) {
                    writeln!(
                        output,
                        "q {} 0 0 {} {} {} cm /{name} Do Q",
                        bounds.width, bounds.height, bounds.x, bottom
                    )
                    .expect("writing to String cannot fail");
                } else {
                    // Keep unresolved images visible rather than silently dropping them.
                    writeln!(
                        output,
                        "q 0.45 G 0.8 w [4 3] 0 d {} {} {} {} re S Q",
                        bounds.x, bottom, bounds.width, bounds.height
                    )
                    .expect("writing to String cannot fail");
                    let label = if alt_text.is_empty() {
                        "Image not embedded"
                    } else {
                        alt_text
                    };
                    writeln!(
                        output,
                        "BT /F1 10 Tf 0.35 0.35 0.35 rg 1 0 0 1 {} {} Tm <{}> Tj ET",
                        bounds.x + 4.0,
                        bottom + (bounds.height / 2.0).max(10.0),
                        pdf_hex_string(label)
                    )
                    .expect("writing to String cannot fail");
                }
            }
        }
    }
    Ok(output)
}

fn pdf_rgb(color: u32) -> String {
    format!(
        "{:.5} {:.5} {:.5}",
        ((color >> 16) & 0xff) as f32 / 255.0,
        ((color >> 8) & 0xff) as f32 / 255.0,
        (color & 0xff) as f32 / 255.0
    )
}

fn pdf_hex_string(value: &str) -> String {
    value
        .chars()
        .map(pdf_win_ansi_byte)
        .map(|byte| format!("{byte:02X}"))
        .collect()
}

fn pdf_win_ansi_byte(character: char) -> u8 {
    let code = character as u32;
    if code <= 0x7f || (0xa0..=0xff).contains(&code) {
        return code as u8;
    }
    match character {
        '€' => 0x80,
        '‚' => 0x82,
        'ƒ' => 0x83,
        '„' => 0x84,
        '…' => 0x85,
        '†' => 0x86,
        '‡' => 0x87,
        'ˆ' => 0x88,
        '‰' => 0x89,
        'Š' => 0x8a,
        '‹' => 0x8b,
        'Œ' => 0x8c,
        'Ž' => 0x8e,
        '‘' => 0x91,
        '’' => 0x92,
        '“' => 0x93,
        '”' => 0x94,
        '•' => 0x95,
        '–' => 0x96,
        '—' => 0x97,
        '˜' => 0x98,
        '™' => 0x99,
        'š' => 0x9a,
        '›' => 0x9b,
        'œ' => 0x9c,
        'ž' => 0x9e,
        'Ÿ' => 0x9f,
        _ => b'?',
    }
}

fn wrap_text_for_svg(text: &str, max_width: f32, font_size: f32) -> Vec<String> {
    let max_chars = ((max_width / (font_size * 0.55)).floor() as usize).max(1);
    let mut output = Vec::new();
    for explicit_line in text.split('\n') {
        let mut line = String::new();
        for word in explicit_line.split_whitespace() {
            let word_len = word.chars().count();
            let line_len = line.chars().count();
            if !line.is_empty() && line_len + 1 + word_len <= max_chars {
                line.push(' ');
                line.push_str(word);
            } else {
                if !line.is_empty() {
                    output.push(std::mem::take(&mut line));
                }
                let mut chars = word.chars().peekable();
                while chars.peek().is_some() {
                    let chunk: String = chars.by_ref().take(max_chars).collect();
                    if chars.peek().is_some() {
                        output.push(chunk);
                    } else {
                        line = chunk;
                    }
                }
            }
        }
        output.push(line);
    }
    output
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Export editable text and primitive shapes to a basic PowerPoint OOXML package.
/// Image references become relinkable placeholders; file access and image processing
/// remain the responsibility of the host application. Speaker notes and animations
/// are not part of this initial export subset.
pub fn export_pptx(presentation: &Presentation) -> io::Result<Vec<u8>> {
    presentation.validate()?;
    if presentation.slides.is_empty() {
        return Err(invalid_input("a PPTX must contain at least one slide"));
    }
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let mut types = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
<Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
<Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
"#,
    );
    for i in 0..presentation.slides.len() {
        let _ = write!(
            types,
            r#"<Override PartName="/ppt/slides/slide{}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#,
            i + 1
        );
    }
    types.push_str("</Types>");

    let mut pres = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
<p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst><p:sldIdLst>"#,
    );
    let mut pres_rels = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="slideMasters/slideMaster1.xml"/>"#,
    );
    for i in 0..presentation.slides.len() {
        let _ = write!(pres, r#"<p:sldId id="{}" r:id="rId{}"/>"#, 256 + i, i + 2);
        let _ = write!(
            pres_rels,
            r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{}.xml"/>"#,
            i + 2,
            i + 1
        );
    }
    let _ = write!(
        pres,
        r#"</p:sldIdLst><p:sldSz cx="{}" cy="{}" type="custom"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>"#,
        points_to_emu(presentation.width),
        points_to_emu(presentation.height)
    );
    pres_rels.push_str("</Relationships>");

    let root_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#;
    let master = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="1" r:id="rId1"/></p:sldLayoutIdLst><p:txStyles><p:titleStyle/><p:bodyStyle/><p:otherStyle/></p:txStyles></p:sldMaster>"#;
    let master_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/></Relationships>"#;
    let layout = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="blank" preserve="1"><p:cSld name="Blank"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#;
    let layout_rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#;
    let theme = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Likounsee"><a:themeElements><a:clrScheme name="Likounsee"><a:dk1><a:srgbClr val="000000"/></a:dk1><a:lt1><a:srgbClr val="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="1F1F1F"/></a:dk2><a:lt2><a:srgbClr val="EEEEEE"/></a:lt2><a:accent1><a:srgbClr val="4472C4"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2><a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4><a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme name="Likounsee"><a:majorFont><a:latin typeface="Arial"/></a:majorFont><a:minorFont><a:latin typeface="Arial"/></a:minorFont></a:fontScheme><a:fmtScheme name="Likounsee"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst><a:lnStyleLst><a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>"#;

    write_pptx_part(&mut zip, "[Content_Types].xml", types.as_bytes(), options)?;
    write_pptx_part(&mut zip, "_rels/.rels", root_rels.as_bytes(), options)?;
    write_pptx_part(&mut zip, "ppt/presentation.xml", pres.as_bytes(), options)?;
    write_pptx_part(
        &mut zip,
        "ppt/_rels/presentation.xml.rels",
        pres_rels.as_bytes(),
        options,
    )?;
    write_pptx_part(
        &mut zip,
        "ppt/slideMasters/slideMaster1.xml",
        master.as_bytes(),
        options,
    )?;
    write_pptx_part(
        &mut zip,
        "ppt/slideMasters/_rels/slideMaster1.xml.rels",
        master_rels.as_bytes(),
        options,
    )?;
    write_pptx_part(
        &mut zip,
        "ppt/slideLayouts/slideLayout1.xml",
        layout.as_bytes(),
        options,
    )?;
    write_pptx_part(
        &mut zip,
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
        layout_rels.as_bytes(),
        options,
    )?;
    write_pptx_part(&mut zip, "ppt/theme/theme1.xml", theme.as_bytes(), options)?;
    for (i, slide) in presentation.slides.iter().enumerate() {
        let xml = pptx_slide_xml(slide)?;
        write_pptx_part(
            &mut zip,
            &format!("ppt/slides/slide{}.xml", i + 1),
            xml.as_bytes(),
            options,
        )?;
        let rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/></Relationships>"#;
        write_pptx_part(
            &mut zip,
            &format!("ppt/slides/_rels/slide{}.xml.rels", i + 1),
            rels.as_bytes(),
            options,
        )?;
    }
    zip.finish()
        .map(|cursor| cursor.into_inner())
        .map_err(|error| io::Error::other(format!("PPTX ZIP finalization failed: {error}")))
}

fn write_pptx_part(
    zip: &mut ZipWriter<Cursor<Vec<u8>>>,
    path: &str,
    bytes: &[u8],
    options: FileOptions,
) -> io::Result<()> {
    zip.start_file(path, options)
        .map_err(|error| io::Error::other(format!("PPTX ZIP entry failed: {error}")))?;
    zip.write_all(bytes)
}

fn points_to_emu(points: f32) -> u64 {
    (f64::from(points) * 12_700.0).round() as u64
}

fn pptx_slide_xml(slide: &Slide) -> io::Result<String> {
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld name=""#,
    );
    xml.push_str(&xml_escape(&slide.name));
    xml.push_str(r#""><p:bg><p:bgPr><a:solidFill><a:srgbClr val=""#);
    let _ = write!(xml, "{:06X}", slide.background);
    xml.push_str(r#""/></a:solidFill><a:effectLst/></p:bgPr></p:bg><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>"#);
    for (index, element) in slide.elements.iter().enumerate() {
        let id = index + 2;
        let b = element.bounds;
        let description = match &element.kind {
            ElementKind::Text { text, .. } => xml_escape(text),
            ElementKind::Image { alt_text, .. } => xml_escape(alt_text),
            ElementKind::Shape { .. } => String::new(),
        };
        let _ = write!(
            xml,
            r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="Object {id}" descr="{description}"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="{}" y="{}"/><a:ext cx="{}" cy="{}"/></a:xfrm>"#,
            points_to_emu(b.x),
            points_to_emu(b.y),
            points_to_emu(b.width),
            points_to_emu(b.height)
        );
        match &element.kind {
            ElementKind::Text { text, style } => {
                xml.push_str(r#"<a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:noFill/></p:spPr><p:txBody><a:bodyPr wrap="square"/><a:lstStyle/><a:p><a:pPr algn=""#);
                xml.push_str(match style.alignment {
                    TextAlignment::Left => "l",
                    TextAlignment::Center => "ctr",
                    TextAlignment::Right => "r",
                    TextAlignment::Justify => "just",
                });
                let _ = write!(
                    xml,
                    r#""/><a:r><a:rPr lang="en-US" sz="{}" b="{}" i="{}" u="{}"><a:solidFill><a:srgbClr val="{:06X}"/></a:solidFill><a:latin typeface="{}"/></a:rPr><a:t xml:space="preserve">{}</a:t></a:r><a:endParaRPr lang="en-US"/></a:p></p:txBody></p:sp>"#,
                    (style.font_size * 100.0).round() as u32,
                    if style.bold { 1 } else { 0 },
                    if style.italic { 1 } else { 0 },
                    if style.underline { "sng" } else { "none" },
                    style.color,
                    xml_escape(&style.font_family),
                    xml_escape(text)
                );
            }
            ElementKind::Shape {
                shape,
                fill,
                stroke,
                stroke_width,
            } => {
                let geom = match shape {
                    ShapeKind::Rectangle => "rect",
                    ShapeKind::Ellipse => "ellipse",
                    ShapeKind::Line => "line",
                    ShapeKind::Arrow => "rightArrow",
                };
                let _ = write!(xml, r#"<a:prstGeom prst="{geom}"><a:avLst/></a:prstGeom>"#);
                if let Some(color) = fill {
                    let _ = write!(
                        xml,
                        r#"<a:solidFill><a:srgbClr val="{color:06X}"/></a:solidFill>"#
                    );
                } else {
                    xml.push_str("<a:noFill/>");
                }
                if let Some(color) = stroke {
                    let _ = write!(
                        xml,
                        r#"<a:ln w="{}"><a:solidFill><a:srgbClr val="{color:06X}"/></a:solidFill></a:ln>"#,
                        (stroke_width * 12_700.0).round() as u64
                    );
                } else {
                    xml.push_str("<a:ln><a:noFill/></a:ln>");
                }
                xml.push_str(r#"</p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:endParaRPr lang="en-US"/></a:p></p:txBody></p:sp>"#);
            }
            ElementKind::Image { source, alt_text } => {
                xml.push_str(r#"<a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="E7E6E6"/></a:solidFill><a:ln><a:solidFill><a:srgbClr val="7F7F7F"/></a:solidFill></a:ln></p:spPr><p:txBody><a:bodyPr wrap="square"/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US"/><a:t>"#);
                let image_label = format!("{alt_text} — {source}");
                xml.push_str(&xml_escape(&image_label));
                xml.push_str(r#"</a:t></a:r><a:endParaRPr lang="en-US"/></a:p></p:txBody></p:sp>"#);
            }
        }
    }
    xml.push_str("</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>");
    Ok(xml)
}

fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\u{9}'
            | '\u{A}'
            | '\u{D}'
            | '\u{20}'..='\u{D7FF}'
            | '\u{E000}'..='\u{FFFD}'
            | '\u{10000}'..='\u{10FFFF}' => out.push(character),
            _ => out.push('\u{FFFD}'),
        }
    }
    out
}

fn encode_presentation(presentation: &Presentation) -> io::Result<Vec<u8>> {
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
    fn changes_canvas_dimensions_and_preserves_them_in_serialization() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        deck.set_dimensions(720.0, 540.0).unwrap();
        assert_eq!((deck.width(), deck.height()), (720.0, 540.0));
        let revision = deck.revision();
        deck.set_dimensions(720.0, 540.0).unwrap();
        assert_eq!(deck.revision(), revision);
        let restored = decode_presentation(&encode_presentation(&deck).unwrap()).unwrap();
        assert_eq!((restored.width(), restored.height()), (720.0, 540.0));
        assert!(deck.set_dimensions(f32::NAN, 540.0).is_err());
        assert_eq!((deck.width(), deck.height()), (720.0, 540.0));
        assert_eq!(deck.revision(), revision);
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
        let text = deck
            .add_text(
                slide,
                rect(0.0, 0.0, 100.0, 40.0),
                "Title",
                TextStyle::default(),
            )
            .unwrap();
        let shape = deck
            .add_shape(
                slide,
                rect(0.0, 50.0, 100.0, 40.0),
                ShapeKind::Rectangle,
                None,
                None,
                1.0,
            )
            .unwrap();
        let image = deck
            .add_image(slide, rect(0.0, 100.0, 100.0, 80.0), "old.png", "Old")
            .unwrap();

        let mut style = TextStyle::new("Inter", 36.0, 0x123456).unwrap();
        style.set_emphasis(true, false, true);
        style.set_alignment(TextAlignment::Center);
        deck.set_text_style(slide, text, style.clone()).unwrap();
        deck.set_shape_style(slide, shape, Some(0xaabbcc), Some(0x010203), 3.5)
            .unwrap();
        deck.update_image(slide, image, "new.png", "Updated image")
            .unwrap();

        assert_eq!(deck.slide(slide).unwrap().name(), "Introduction");
        assert!(
            matches!(deck.slide(slide).unwrap().element(text).unwrap().kind(), ElementKind::Text { style: actual, .. } if actual == &style)
        );
        assert!(
            matches!(deck.slide(slide).unwrap().element(shape).unwrap().kind(), ElementKind::Shape { fill: Some(0xaabbcc), stroke: Some(0x010203), stroke_width, .. } if *stroke_width == 3.5)
        );
        assert!(
            matches!(deck.slide(slide).unwrap().element(image).unwrap().kind(), ElementKind::Image { source, alt_text } if source == "new.png" && alt_text == "Updated image")
        );

        let restored = decode_presentation(&encode_presentation(&deck).unwrap()).unwrap();
        assert_eq!(restored, deck);
    }

    #[test]
    fn creates_and_duplicates_all_element_types_with_independent_ids() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let slide = deck.slides()[0].id();
        let text = deck
            .add_text(
                slide,
                rect(10.0, 10.0, 200.0, 50.0),
                "Title",
                TextStyle::default(),
            )
            .unwrap();
        let shape = deck
            .add_shape(
                slide,
                rect(10.0, 70.0, 100.0, 100.0),
                ShapeKind::Ellipse,
                Some(0x123456),
                None,
                0.0,
            )
            .unwrap();
        let image = deck
            .add_image(
                slide,
                rect(120.0, 70.0, 100.0, 100.0),
                "assets/photo.png",
                "Photo",
            )
            .unwrap();

        let duplicate = deck.duplicate_element(slide, text).unwrap();
        let elements = deck.slide(slide).unwrap().elements();
        assert_eq!(elements.len(), 4);
        assert_eq!(elements[0].id(), text);
        assert_eq!(elements[1].id(), duplicate);
        assert_ne!(elements[1].id(), elements[0].id());
        assert_eq!(elements[1].kind(), elements[0].kind());
        assert!(deck.slide(slide).unwrap().element(shape).is_some());
        assert!(deck.slide(slide).unwrap().element(image).is_some());

        let restored = decode_presentation(&encode_presentation(&deck).unwrap()).unwrap();
        assert_eq!(restored, deck);
    }

    #[test]
    fn built_in_slide_layouts_create_editable_placeholders_and_round_trip() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let title = deck
            .add_slide_with_layout("Title slide", SlideLayout::Title)
            .unwrap();
        let columns = deck
            .add_slide_with_layout("Comparison", SlideLayout::TwoColumns)
            .unwrap();
        let blank = deck
            .add_slide_with_layout("Blank", SlideLayout::Blank)
            .unwrap();

        assert_eq!(deck.slide(title).unwrap().elements().len(), 2);
        assert_eq!(deck.slide(columns).unwrap().elements().len(), 3);
        assert!(deck.slide(blank).unwrap().elements().is_empty());
        assert!(
            deck.slide(title)
                .unwrap()
                .elements()
                .iter()
                .all(|element| { matches!(element.kind(), ElementKind::Text { .. }) })
        );
        let restored = decode_presentation(&encode_presentation(&deck).unwrap()).unwrap();
        assert_eq!(restored, deck);
    }

    #[test]
    fn built_in_layouts_support_minimum_canvas_dimensions() {
        let mut deck = Presentation::new("Small", 1.0, 1.0).unwrap();
        for layout in [
            SlideLayout::Title,
            SlideLayout::TitleAndContent,
            SlideLayout::TwoColumns,
            SlideLayout::SectionHeader,
            SlideLayout::TitleOnly,
            SlideLayout::Comparison,
            SlideLayout::Quote,
            SlideLayout::ContentWithSidebar,
        ] {
            let slide = deck.add_slide_with_layout("Small layout", layout).unwrap();
            assert!(!deck.slide(slide).unwrap().elements().is_empty());
        }
        assert_eq!(
            decode_presentation(&encode_presentation(&deck).unwrap()).unwrap(),
            deck
        );
    }

    #[test]
    fn advanced_layouts_create_expected_editable_placeholders() {
        let mut deck = Presentation::new("Advanced layouts", 1280.0, 720.0).unwrap();
        let cases = [
            (SlideLayout::TitleOnly, 1),
            (SlideLayout::Comparison, 5),
            (SlideLayout::Quote, 2),
            (SlideLayout::ContentWithSidebar, 3),
        ];
        for (layout, expected_count) in cases {
            let id = deck.add_slide_with_layout("Preset", layout).unwrap();
            let slide = deck.slide(id).unwrap();
            assert_eq!(slide.elements().len(), expected_count);
            assert!(
                slide
                    .elements()
                    .iter()
                    .all(|element| matches!(element.kind(), ElementKind::Text { .. }))
            );
        }
        let restored = decode_presentation(&encode_presentation(&deck).unwrap()).unwrap();
        assert_eq!(restored, deck);
    }

    struct TestPngRasterizer {
        calls: std::cell::Cell<usize>,
    }

    impl TestPngRasterizer {
        fn new() -> Self {
            Self {
                calls: std::cell::Cell::new(0),
            }
        }
    }

    impl PngRasterizer for TestPngRasterizer {
        fn rasterize_svg(&self, svg: &str, width: u32, height: u32) -> io::Result<Vec<u8>> {
            assert!(svg.starts_with("<svg "));
            self.calls.set(self.calls.get() + 1);
            Ok(test_png(width, height))
        }
    }

    fn test_png(width: u32, height: u32) -> Vec<u8> {
        fn append_chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            output.extend_from_slice(&(data.len() as u32).to_be_bytes());
            output.extend_from_slice(kind);
            output.extend_from_slice(data);
            let mut crc_input = kind.to_vec();
            crc_input.extend_from_slice(data);
            output.extend_from_slice(&crc32fast::hash(&crc_input).to_be_bytes());
        }

        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut header = Vec::new();
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&[8, 6, 0, 0, 0]);
        append_chunk(&mut png, b"IHDR", &header);
        // A valid zlib stream containing one transparent RGBA scanline.
        let raw = [0, 0, 0, 0, 0];
        let mut compressed = vec![0x78, 0x01, 0x01, 0x05, 0x00, 0xfa, 0xff];
        compressed.extend_from_slice(&raw);
        let (adler_a, adler_b) = raw.iter().fold((1u32, 0u32), |(a, b), byte| {
            let a = (a + u32::from(*byte)) % 65521;
            (a, (b + a) % 65521)
        });
        compressed.extend_from_slice(&((adler_b << 16) | adler_a).to_be_bytes());
        append_chunk(&mut png, b"IDAT", &compressed);
        append_chunk(&mut png, b"IEND", &[]);
        png
    }

    #[test]
    fn png_adapter_exports_one_slide_and_checks_renderer_output() {
        let deck = Presentation::new("PNG adapter", 800.0, 450.0).unwrap();
        let slide = deck.slides()[0].id();
        let rasterizer = TestPngRasterizer::new();
        let png = render_slide_png_with(&deck, slide, 1600, 900, &rasterizer).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(rasterizer.calls.get(), 1);
        assert!(render_slide_png_with(&deck, 999, 1600, 900, &rasterizer).is_err());
        assert!(render_slide_png_with(&deck, slide, 0, 900, &rasterizer).is_err());
        assert_eq!(rasterizer.calls.get(), 1);
    }

    #[test]
    fn png_adapter_exports_all_slides_in_order_and_rejects_bad_pngs() {
        let mut deck = Presentation::new("PNG batch", 640.0, 360.0).unwrap();
        let first = deck.slides()[0].id();
        let second = deck.add_slide("Second").unwrap();
        let third = deck.add_slide("Third").unwrap();
        let rasterizer = TestPngRasterizer::new();
        let rendered = render_presentation_png_with(&deck, 320, 180, &rasterizer).unwrap();
        assert_eq!(
            rendered.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![first, second, third]
        );
        assert_eq!(rasterizer.calls.get(), 3);
        assert!(rendered.iter().all(|(_, png)| png.starts_with(b"\x89PNG")));

        let mut corrupt = test_png(320, 180);
        let idat = corrupt
            .windows(4)
            .position(|window| window == b"IDAT")
            .unwrap();
        corrupt[idat + 5] ^= 0x40;
        assert!(validate_png(&corrupt, 320, 180).is_err());
        assert!(validate_png(&test_png(320, 180), 640, 180).is_err());
        assert!(validate_png(&test_png(320, 180), 0, 180).is_err());
        assert!(validate_png(b"not png", 320, 180).is_err());

        let mut invalid_ihdr = test_png(320, 180);
        let ihdr_type = invalid_ihdr
            .windows(4)
            .position(|window| window == b"IHDR")
            .unwrap();
        let bit_depth = ihdr_type + 4 + 8;
        invalid_ihdr[bit_depth] = 3;
        let ihdr_crc = ihdr_type + 4 + 13;
        let crc = crc32fast::hash(&invalid_ihdr[ihdr_type..ihdr_crc]);
        invalid_ihdr[ihdr_crc..ihdr_crc + 4].copy_from_slice(&crc.to_be_bytes());
        assert!(validate_png(&invalid_ihdr, 320, 180).is_err());

        let mut non_consecutive_idat = test_png(320, 180);
        let idat_type = non_consecutive_idat
            .windows(4)
            .position(|window| window == b"IDAT")
            .unwrap();
        let idat_length = u32::from_be_bytes(
            non_consecutive_idat[idat_type - 4..idat_type]
                .try_into()
                .unwrap(),
        ) as usize;
        let idat_end = idat_type + 4 + idat_length + 4;
        let duplicate_idat = non_consecutive_idat[idat_type - 4..idat_end].to_vec();
        let iend_type = non_consecutive_idat
            .windows(4)
            .position(|window| window == b"IEND")
            .unwrap();
        let ancillary = {
            let mut chunk = Vec::new();
            chunk.extend_from_slice(&0u32.to_be_bytes());
            chunk.extend_from_slice(b"tEXt");
            chunk.extend_from_slice(&crc32fast::hash(b"tEXt").to_be_bytes());
            chunk
        };
        non_consecutive_idat.splice(iend_type - 4..iend_type - 4, ancillary);
        let iend_type = non_consecutive_idat
            .windows(4)
            .position(|window| window == b"IEND")
            .unwrap();
        non_consecutive_idat.splice(iend_type - 4..iend_type - 4, duplicate_idat);
        assert!(validate_png(&non_consecutive_idat, 320, 180).is_err());
    }

    #[test]
    fn pdf_export_writes_valid_single_page_document_and_preserves_text() {
        let mut deck = Presentation::new("PDF résumé", 800.0, 450.0).unwrap();
        let slide = deck.slides()[0].id();
        let mut style = TextStyle::new("Inter", 28.0, 0x123456).unwrap();
        style.set_emphasis(true, true, true);
        style.set_alignment(TextAlignment::Center);
        deck.add_text(
            slide,
            rect(20.0, 30.0, 300.0, 70.0),
            "Bonjour € — été",
            style,
        )
        .unwrap();
        deck.add_shape(
            slide,
            rect(40.0, 120.0, 100.0, 80.0),
            ShapeKind::Ellipse,
            Some(0xaabbcc),
            Some(0x010203),
            2.0,
        )
        .unwrap();

        let pdf = render_slide_pdf(&deck, slide).unwrap();
        let content = String::from_utf8_lossy(&pdf);
        assert!(content.starts_with("%PDF-1.4"));
        assert!(content.contains("/Type /Catalog"));
        assert!(content.contains("/Count 1"));
        assert!(content.contains("/Helvetica-BoldOblique"));
        assert!(content.contains("/Times-BoldItalic"));
        assert!(content.contains("/Courier-BoldOblique"));
        assert!(content.contains("426F6E6A6F75722080209720E974E9"));
        assert!(content.contains(" re W n"));
        assert!(content.ends_with("%%EOF\n"));
        assert!(render_slide_pdf(&deck, 999).is_err());
    }

    #[test]
    fn pdf_font_selection_maps_common_families_and_falls_back_safely() {
        assert_eq!(pdf_font_resource("Arial", false, false), "F1");
        assert_eq!(pdf_font_resource("Times New Roman", true, true), "F8");
        assert_eq!(pdf_font_resource("Georgia", true, false), "F6");
        assert_eq!(pdf_font_resource("sans-serif", false, false), "F1");
        assert_eq!(pdf_font_resource("Cambria", false, false), "F5");
        assert_eq!(pdf_font_resource("Consolas", false, true), "F11");
        assert_eq!(pdf_font_resource("Courier New", true, false), "F10");
        assert_eq!(pdf_font_resource("Inter Display", false, false), "F1");
    }

    struct TestJpegProvider;

    impl PdfImageProvider for TestJpegProvider {
        fn resolve_jpeg(&self, source: &str) -> io::Result<Option<PdfJpegImage>> {
            if source != "photo.jpg" {
                return Ok(None);
            }
            // Minimal SOF-bearing fixture; this test verifies container embedding,
            // not JPEG decoding (which deliberately belongs to the host application).
            Ok(Some(PdfJpegImage {
                bytes: vec![
                    0xff, 0xd8, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x01, 0x00, 0x01, 0x03, 0x01,
                    0x11, 0x00, 0xff, 0xd9,
                ],
                width: 1,
                height: 1,
                color_space: PdfImageColorSpace::Rgb,
            }))
        }
    }

    #[test]
    fn pptx_export_creates_open_xml_package_with_editable_content() {
        use std::io::Read;
        let mut deck = Presentation::new("Quarterly & annual", 960.0, 540.0).unwrap();
        let slide = deck.slides()[0].id();
        deck.rename_slide(slide, "Overview & Results").unwrap();
        let mut title_style = TextStyle::new("Arial", 32.0, 0x123456).unwrap();
        title_style.set_emphasis(true, false, false);
        title_style.set_alignment(TextAlignment::Center);
        deck.add_text(
            slide,
            rect(40.0, 30.0, 500.0, 80.0),
            "Revenue < 2026 & growth",
            title_style,
        )
        .unwrap();
        deck.add_shape(
            slide,
            rect(50.0, 140.0, 200.0, 100.0),
            ShapeKind::Rectangle,
            Some(0xAABBCC),
            Some(0x112233),
            2.0,
        )
        .unwrap();
        deck.add_image(
            slide,
            rect(300.0, 140.0, 100.0, 80.0),
            "assets/chart.png",
            "Chart preview",
        )
        .unwrap();
        deck.set_slide_notes(slide, "Speaker notes stay in native format for now")
            .unwrap();
        deck.add_slide("Second slide").unwrap();

        let bytes = export_pptx(&deck).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut presentation = String::new();
        archive
            .by_name("ppt/presentation.xml")
            .unwrap()
            .read_to_string(&mut presentation)
            .unwrap();
        assert!(presentation.contains("<p:sldId "));
        assert!(presentation.contains("cx=\"12192000\" cy=\"6858000\""));

        let mut slide_xml = String::new();
        archive
            .by_name("ppt/slides/slide1.xml")
            .unwrap()
            .read_to_string(&mut slide_xml)
            .unwrap();
        assert!(slide_xml.contains("name=\"Overview &amp; Results\""));
        assert!(slide_xml.contains("descr=\"Revenue &lt; 2026 &amp; growth\""));
        assert!(slide_xml.contains("descr=\"Chart preview\""));
        assert!(slide_xml.contains("Revenue &lt; 2026 &amp; growth"));
        assert!(slide_xml.contains("typeface=\"Arial\""));
        assert!(slide_xml.contains("prst=\"rect\""));
        assert!(slide_xml.contains("Chart preview — assets/chart.png"));
        assert!(archive.by_name("ppt/slides/slide2.xml").is_ok());
        assert!(archive.by_name("[Content_Types].xml").is_ok());
    }

    #[test]
    fn pptx_export_rejects_invalid_models_before_writing_archive() {
        let mut deck = Presentation::new("Invalid", 960.0, 540.0).unwrap();
        deck.width = f32::NAN;
        assert!(export_pptx(&deck).is_err());
    }

    #[test]
    fn pdf_export_embeds_provider_jpegs_as_shared_image_xobjects() {
        let mut deck = Presentation::new("JPEG deck", 640.0, 360.0).unwrap();
        let first = deck.slides()[0].id();
        deck.add_image(first, rect(10.0, 20.0, 100.0, 80.0), "photo.jpg", "Photo")
            .unwrap();
        deck.add_image(
            first,
            rect(120.0, 20.0, 100.0, 80.0),
            "photo.jpg",
            "Photo copy",
        )
        .unwrap();
        let second = deck.add_slide("Second").unwrap();
        deck.add_image(second, rect(10.0, 10.0, 50.0, 50.0), "missing.jpg", "")
            .unwrap();

        let pdf = render_presentation_pdf_with_images(&deck, &TestJpegProvider).unwrap();
        let content = String::from_utf8_lossy(&pdf);
        assert!(content.contains("/Subtype /Image"));
        assert!(content.contains("/ColorSpace /DeviceRGB"));
        assert!(content.contains("/Filter /DCTDecode"));
        assert!(content.contains("/Im1 Do"));
        assert!(content.contains("496D616765206E6F7420656D626564646564"));
        assert_eq!(content.matches("/Subtype /Image").count(), 1);
    }

    #[test]
    fn pdf_slide_export_embeds_images_and_cmyk_uses_inverted_decode() {
        struct CmykProvider;
        impl PdfImageProvider for CmykProvider {
            fn resolve_jpeg(&self, _source: &str) -> io::Result<Option<PdfJpegImage>> {
                Ok(Some(PdfJpegImage {
                    bytes: vec![
                        0xff, 0xd8, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x01, 0x00, 0x01, 0x04,
                        0x01, 0x11, 0x00, 0xff, 0xd9,
                    ],
                    width: 1,
                    height: 1,
                    color_space: PdfImageColorSpace::Cmyk,
                }))
            }
        }
        let mut deck = Presentation::new("CMYK slide", 320.0, 200.0).unwrap();
        let slide = deck.slides()[0].id();
        deck.add_image(slide, rect(0.0, 0.0, 80.0, 60.0), "cmyk.jpg", "CMYK")
            .unwrap();
        let pdf = render_slide_pdf_with_images(&deck, slide, &CmykProvider).unwrap();
        let content = String::from_utf8_lossy(&pdf);
        assert!(content.contains("/ColorSpace /DeviceCMYK"));
        assert!(content.contains("/Decode [1 0 1 0 1 0 1 0]"));
        assert!(content.contains("/Im1 Do"));
    }

    #[test]
    fn pdf_jpeg_metadata_must_match_the_frame_header() {
        let image = PdfJpegImage {
            bytes: vec![
                0xff, 0xd8, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x01, 0x00, 0x01, 0x03, 0x01, 0x11,
                0x00, 0xff, 0xd9,
            ],
            width: 2,
            height: 1,
            color_space: PdfImageColorSpace::Rgb,
        };
        assert!(image.validate().is_err());
    }

    #[test]
    fn pdf_export_preserves_slide_order_and_marks_external_images() {
        let mut deck = Presentation::new("PDF deck", 640.0, 360.0).unwrap();
        let first = deck.slides()[0].id();
        deck.add_image(
            first,
            rect(20.0, 20.0, 160.0, 90.0),
            "external/photo.png",
            "Photo non intégrée",
        )
        .unwrap();
        deck.add_slide("Second").unwrap();

        let pdf = render_presentation_pdf(&deck).unwrap();
        let content = String::from_utf8_lossy(&pdf);
        assert!(content.contains("/Count 2"));
        assert!(content.contains("/Kids [15 0 R 17 0 R]"));
        assert!(content.contains("50686F746F206E6F6E20696E74E96772E965"));
        assert!(content.contains("[4 3] 0 d"));
        assert!(content.contains("/MediaBox [0 0 640 360]"));
    }

    #[test]
    fn svg_renderer_outputs_text_shapes_and_external_images() {
        let mut deck = Presentation::new("SVG & test", 800.0, 450.0).unwrap();
        let slide = deck.slides()[0].id();
        let mut style = TextStyle::new("Inter & Sans", 28.0, 0x123456).unwrap();
        style.set_emphasis(true, true, true);
        style.set_alignment(TextAlignment::Center);
        deck.add_text(
            slide,
            rect(20.0, 30.0, 300.0, 60.0),
            "Hello <world> & everyone",
            style,
        )
        .unwrap();
        deck.add_shape(
            slide,
            rect(40.0, 120.0, 100.0, 80.0),
            ShapeKind::Rectangle,
            Some(0xaabbcc),
            Some(0x010203),
            2.0,
        )
        .unwrap();
        deck.add_shape(
            slide,
            rect(180.0, 120.0, 100.0, 80.0),
            ShapeKind::Ellipse,
            None,
            Some(0x334455),
            3.0,
        )
        .unwrap();
        deck.add_shape(
            slide,
            rect(300.0, 120.0, 100.0, 80.0),
            ShapeKind::Arrow,
            None,
            Some(0x556677),
            2.0,
        )
        .unwrap();
        deck.add_image(
            slide,
            rect(420.0, 120.0, 160.0, 100.0),
            "assets/photo & chart.png",
            "A <chart>",
        )
        .unwrap();
        let svg = render_slide_svg(&deck, slide).unwrap();
        assert!(svg.starts_with("<svg "));
        assert!(svg.contains("SVG &amp; test"));
        assert!(svg.contains(">Hello &lt;world&gt; &amp;</tspan>"));
        assert!(svg.contains(">everyone</tspan>"));
        assert!(svg.contains("font-family=\"Inter &amp; Sans\""));
        assert!(svg.contains("text-decoration=\"underline\""));
        assert!(svg.contains("<rect id=\"element-"));
        assert!(svg.contains("<ellipse id=\"element-"));
        assert!(svg.contains("marker-end=\"url(#arrowhead)\""));
        assert!(svg.contains("assets/photo &amp; chart.png"));
        assert!(svg.contains("<title>A &lt;chart&gt;</title>"));
        assert!(svg.ends_with("</svg>\n"));
    }

    #[test]
    fn svg_renderer_exports_all_slides_in_presentation_order() {
        let mut deck = Presentation::new("Batch SVG", 640.0, 360.0).unwrap();
        let first = deck.slides()[0].id();
        let second = deck.add_slide("Second").unwrap();
        let third = deck.add_slide("Third").unwrap();
        let rendered = render_all_slides_svg(&deck).unwrap();
        assert_eq!(
            rendered.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![first, second, third]
        );
        assert!(rendered[0].1.contains("<title>Slide 1</title>"));
        assert!(rendered[1].1.contains("<title>Second</title>"));
        assert!(rendered[2].1.contains("<title>Third</title>"));
    }

    #[test]
    fn svg_renderer_wraps_long_text_and_clips_to_text_bounds() {
        let mut deck = Presentation::new("Wrapping", 300.0, 160.0).unwrap();
        let slide = deck.slides()[0].id();
        deck.add_text(
            slide,
            rect(10.0, 10.0, 90.0, 120.0),
            "alpha beta gamma extraordinarilylongword",
            TextStyle::default(),
        )
        .unwrap();
        let svg = render_slide_svg(&deck, slide).unwrap();
        assert!(svg.contains("clipPath id=\"clip-"));
        assert!(svg.contains("clip-path=\"url(#clip-"));
        assert!(svg.contains(">alpha</tspan>"));
        assert!(svg.contains(">beta</tspan>"));
        assert!(svg.contains(">gamma</tspan>"));
        assert!(svg.contains(">extrao</tspan>"));
    }

    #[test]
    fn svg_renderer_rejects_missing_slides_and_keeps_newlines() {
        let mut deck = Presentation::new("SVG", 320.0, 200.0).unwrap();
        let slide = deck.slides()[0].id();
        deck.add_text(
            slide,
            rect(10.0, 10.0, 200.0, 100.0),
            "First\nSecond",
            TextStyle::default(),
        )
        .unwrap();
        assert!(render_slide_svg(&deck, 999).is_err());
        let svg = render_slide_svg(&deck, slide).unwrap();
        assert!(svg.contains("<tspan x=\"10\">First</tspan>"));
        assert!(svg.contains("dy=\""));
        assert!(svg.contains(">Second</tspan>"));
    }

    #[test]
    fn applies_global_theme_to_all_slides_and_preserves_element_properties() {
        let mut deck = Presentation::new("Theme test", 960.0, 540.0).unwrap();
        let first = deck.slides()[0].id();
        let title_style = TextStyle::new("Old", 32.0, 0x111111).unwrap();
        let mut body_style = TextStyle::new("Old", 18.0, 0x222222).unwrap();
        body_style.set_emphasis(true, true, false);
        body_style.set_alignment(TextAlignment::Center);
        let title = deck
            .add_text(first, rect(10.0, 10.0, 300.0, 60.0), "Title", title_style)
            .unwrap();
        let body = deck
            .add_text(
                first,
                rect(10.0, 90.0, 300.0, 100.0),
                "Body",
                body_style.clone(),
            )
            .unwrap();
        let shape = deck
            .add_shape(
                first,
                rect(400.0, 90.0, 100.0, 100.0),
                ShapeKind::Rectangle,
                Some(0xabcdef),
                Some(0x010203),
                2.0,
            )
            .unwrap();
        let second = deck.add_slide("Second").unwrap();
        let theme = PresentationTheme::new(
            "Night",
            0x101820,
            "Inter Display",
            "Inter",
            0xf0f0f0,
            0xd0d0d0,
            0x00aacc,
        )
        .unwrap();
        let revision = deck.revision();
        deck.apply_theme(&theme).unwrap();
        assert_eq!(deck.revision(), revision + 1);
        assert_eq!(deck.slide(first).unwrap().background(), 0x101820);
        assert_eq!(deck.slide(second).unwrap().background(), 0x101820);
        assert!(
            matches!(deck.slide(first).unwrap().element(title).unwrap().kind(), ElementKind::Text { style, .. } if style.font_family() == "Inter Display" && style.color() == 0xf0f0f0)
        );
        assert!(
            matches!(deck.slide(first).unwrap().element(body).unwrap().kind(), ElementKind::Text { style, .. } if style.font_family() == "Inter" && style.color() == 0xd0d0d0 && style.bold() && style.italic() && style.alignment() == TextAlignment::Center)
        );
        assert!(
            matches!(deck.slide(first).unwrap().element(shape).unwrap().kind(), ElementKind::Shape { fill: Some(0x00aacc), stroke: Some(0x010203), stroke_width, .. } if *stroke_width == 2.0)
        );
        let restored = decode_presentation(&encode_presentation(&deck).unwrap()).unwrap();
        assert_eq!(restored, deck);
        let unchanged_revision = deck.revision();
        deck.apply_theme(&theme).unwrap();
        assert_eq!(deck.revision(), unchanged_revision);
    }

    #[test]
    fn built_in_theme_presets_are_valid_and_distinct() {
        let themes = [
            PresentationTheme::light(),
            PresentationTheme::dark(),
            PresentationTheme::business(),
            PresentationTheme::high_contrast(),
        ];
        for theme in &themes {
            assert!(!theme.name().is_empty());
            assert!(theme.background() <= 0x00ff_ffff);
            assert!(theme.title_color() <= 0x00ff_ffff);
            assert!(theme.body_color() <= 0x00ff_ffff);
            assert!(theme.accent_color() <= 0x00ff_ffff);
        }
        assert_ne!(themes[0], themes[1]);
        assert_ne!(themes[1], themes[2]);
        assert_ne!(themes[2], themes[3]);
    }

    #[test]
    fn rejects_invalid_themes_without_mutating_the_presentation() {
        let mut deck = Presentation::new("Theme validation", 960.0, 540.0).unwrap();
        let before = deck.clone();
        assert!(PresentationTheme::new("", 0, "Arial", "Arial", 0, 0, 0).is_err());
        assert!(PresentationTheme::new("Bad color", 0x1000000, "Arial", "Arial", 0, 0, 0).is_err());
        let invalid = PresentationTheme {
            name: "Bad".to_owned(),
            background: 0x1000000,
            title_font: "Arial".to_owned(),
            body_font: "Arial".to_owned(),
            title_color: 0,
            body_color: 0,
            accent_color: 0,
        };
        assert!(deck.apply_theme(&invalid).is_err());
        assert_eq!(deck, before);
    }

    #[test]
    fn aligns_selected_elements_without_changing_their_sizes_or_order() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let slide = deck.slides()[0].id();
        let first = deck
            .add_shape(
                slide,
                rect(10.0, 20.0, 100.0, 40.0),
                ShapeKind::Rectangle,
                None,
                None,
                0.0,
            )
            .unwrap();
        let second = deck
            .add_shape(
                slide,
                rect(250.0, 80.0, 60.0, 30.0),
                ShapeKind::Ellipse,
                None,
                None,
                0.0,
            )
            .unwrap();
        let third = deck
            .add_shape(
                slide,
                rect(500.0, 10.0, 80.0, 50.0),
                ShapeKind::Rectangle,
                None,
                None,
                0.0,
            )
            .unwrap();
        let order = deck
            .slide(slide)
            .unwrap()
            .elements()
            .iter()
            .map(SlideElement::id)
            .collect::<Vec<_>>();
        deck.align_elements(
            slide,
            &[first, second, third],
            ElementAlignment::CenterVertical,
        )
        .unwrap();
        let elements = deck.slide(slide).unwrap();
        let first_bounds = elements.element(first).unwrap().bounds();
        let second_bounds = elements.element(second).unwrap().bounds();
        let third_bounds = elements.element(third).unwrap().bounds();
        assert_eq!(first_bounds.y + first_bounds.height / 2.0, 60.0);
        assert_eq!(second_bounds.y + second_bounds.height / 2.0, 60.0);
        assert_eq!(third_bounds.y + third_bounds.height / 2.0, 60.0);
        assert_eq!(
            (first_bounds.width, second_bounds.width, third_bounds.width),
            (100.0, 60.0, 80.0)
        );
        assert_eq!(
            elements
                .elements()
                .iter()
                .map(SlideElement::id)
                .collect::<Vec<_>>(),
            order
        );
    }

    #[test]
    fn aligns_single_and_multiple_elements_to_the_canvas() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let slide = deck.slides()[0].id();
        let first = deck
            .add_shape(
                slide,
                rect(10.0, 20.0, 100.0, 40.0),
                ShapeKind::Rectangle,
                None,
                None,
                0.0,
            )
            .unwrap();
        let second = deck
            .add_shape(
                slide,
                rect(250.0, 80.0, 60.0, 30.0),
                ShapeKind::Ellipse,
                None,
                None,
                0.0,
            )
            .unwrap();

        deck.align_elements_to_canvas(slide, &[first], ElementAlignment::CenterHorizontal)
            .unwrap();
        deck.align_elements_to_canvas(slide, &[first, second], ElementAlignment::Bottom)
            .unwrap();

        let elements = deck.slide(slide).unwrap();
        assert_eq!(elements.element(first).unwrap().bounds().x(), 430.0);
        assert_eq!(elements.element(first).unwrap().bounds().y(), 500.0);
        assert_eq!(elements.element(second).unwrap().bounds().y(), 510.0);
        let restored = decode_presentation(&encode_presentation(&deck).unwrap()).unwrap();
        assert_eq!(restored, deck);
    }

    #[test]
    fn distributes_elements_evenly_and_rejects_invalid_selections_atomically() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let slide = deck.slides()[0].id();
        let first = deck
            .add_shape(
                slide,
                rect(0.0, 0.0, 20.0, 10.0),
                ShapeKind::Rectangle,
                None,
                None,
                0.0,
            )
            .unwrap();
        let middle = deck
            .add_shape(
                slide,
                rect(100.0, 0.0, 20.0, 10.0),
                ShapeKind::Rectangle,
                None,
                None,
                0.0,
            )
            .unwrap();
        let last = deck
            .add_shape(
                slide,
                rect(200.0, 0.0, 20.0, 10.0),
                ShapeKind::Rectangle,
                None,
                None,
                0.0,
            )
            .unwrap();
        deck.distribute_elements(slide, &[last, first, middle], DistributionAxis::Horizontal)
            .unwrap();
        let elements = deck.slide(slide).unwrap();
        assert_eq!(elements.element(first).unwrap().bounds().x(), 0.0);
        assert_eq!(elements.element(middle).unwrap().bounds().x(), 100.0);
        assert_eq!(elements.element(last).unwrap().bounds().x(), 200.0);
        let revision = deck.revision();
        assert!(
            deck.align_elements(slide, &[first, first], ElementAlignment::Left)
                .is_err()
        );
        assert!(
            deck.distribute_elements(slide, &[first, middle], DistributionAxis::Horizontal)
                .is_err()
        );
        assert_eq!(deck.revision(), revision);
        let restored = decode_presentation(&encode_presentation(&deck).unwrap()).unwrap();
        assert_eq!(restored, deck);
    }

    #[test]
    fn invalid_element_updates_are_atomic_and_type_checked() {
        let mut deck = Presentation::new("Deck", 960.0, 540.0).unwrap();
        let slide = deck.slides()[0].id();
        let text = deck
            .add_text(
                slide,
                rect(0.0, 0.0, 100.0, 40.0),
                "Title",
                TextStyle::default(),
            )
            .unwrap();
        let shape = deck
            .add_shape(
                slide,
                rect(0.0, 50.0, 100.0, 40.0),
                ShapeKind::Rectangle,
                None,
                None,
                1.0,
            )
            .unwrap();
        let revision = deck.revision();

        assert!(deck.rename_slide(999, "Missing").is_err());
        assert!(
            deck.set_text_style(slide, shape, TextStyle::default())
                .is_err()
        );
        assert!(deck.set_shape_style(slide, text, None, None, 1.0).is_err());
        assert!(
            deck.set_shape_style(slide, shape, Some(0x1000000), None, 1.0)
                .is_err()
        );
        assert!(
            deck.update_image(slide, text, "image.png", "not an image")
                .is_err()
        );
        assert!(deck.update_image(slide, shape, "", "empty").is_err());
        assert_eq!(deck.revision(), revision);
        assert_eq!(
            deck.slide(slide).unwrap().element(text).unwrap().kind(),
            &ElementKind::Text {
                text: "Title".to_owned(),
                style: TextStyle::default()
            }
        );
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
