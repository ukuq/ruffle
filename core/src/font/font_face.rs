use crate::drawing::Drawing;
use crate::font::{FontMetrics, Glyph};
use crate::prelude::*;
use ruffle_render::bitmap::{Bitmap, BitmapFormat};
use ruffle_render::shape_utils::{DrawCommand, FillRule};

use std::cell::OnceCell;
use std::sync::Arc;
use swf::FillStyle;

struct GlyphToDrawing<'a>(&'a mut Drawing);

struct GlyphToCommands<'a> {
    commands: &'a mut Vec<DrawCommand>,
    contour_start: Option<Point<Twips>>,
    cursor: Point<Twips>,
    transform: ttf_parser::Transform,
}

/// Convert from a TTF outline, to a flash Drawing.
///
/// Note that the Y axis is flipped. I do not know why, but Flash does this.
impl ttf_parser::OutlineBuilder for GlyphToDrawing<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.draw_command(DrawCommand::MoveTo(Point::new(
            Twips::new(x as i32),
            Twips::new(-y as i32),
        )));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.0.draw_command(DrawCommand::LineTo(Point::new(
            Twips::new(x as i32),
            Twips::new(-y as i32),
        )));
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.0.draw_command(DrawCommand::QuadraticCurveTo {
            control: Point::new(Twips::new(x1 as i32), Twips::new(-y1 as i32)),
            anchor: Point::new(Twips::new(x as i32), Twips::new(-y as i32)),
        });
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.0.draw_command(DrawCommand::CubicCurveTo {
            control_a: Point::new(Twips::new(x1 as i32), Twips::new(-y1 as i32)),
            control_b: Point::new(Twips::new(x2 as i32), Twips::new(-y2 as i32)),
            anchor: Point::new(Twips::new(x as i32), Twips::new(-y as i32)),
        });
    }

    fn close(&mut self) {
        self.0.close_path();
    }
}

impl GlyphToCommands<'_> {
    fn point(&self, mut x: f32, mut y: f32) -> Point<Twips> {
        let transform = self.transform;
        let tx = x;
        let ty = y;
        x = transform.a * tx + transform.c * ty + transform.e;
        y = transform.b * tx + transform.d * ty + transform.f;
        Point::new(Twips::new(x as i32), Twips::new(-y as i32))
    }

    fn push(&mut self, command: DrawCommand) {
        self.cursor = command.end_point();
        self.commands.push(command);
    }
}

impl ttf_parser::OutlineBuilder for GlyphToCommands<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let point = self.point(x, y);
        self.contour_start = Some(point);
        self.push(DrawCommand::MoveTo(point));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.push(DrawCommand::LineTo(self.point(x, y)));
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.push(DrawCommand::QuadraticCurveTo {
            control: self.point(x1, y1),
            anchor: self.point(x, y),
        });
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.push(DrawCommand::CubicCurveTo {
            control_a: self.point(x1, y1),
            control_b: self.point(x2, y2),
            anchor: self.point(x, y),
        });
    }

    fn close(&mut self) {
        if let Some(start) = self.contour_start
            && self.cursor != start
        {
            self.push(DrawCommand::LineTo(start));
        }
        self.contour_start = None;
    }
}

struct ColorGlyphPainter<'a, 'face> {
    face: &'face ttf_parser::Face<'a>,
    commands: Vec<DrawCommand>,
    drawing: Drawing,
    has_paint: bool,
    transforms: Vec<ttf_parser::Transform>,
}

impl<'a, 'face> ColorGlyphPainter<'a, 'face> {
    fn new(face: &'face ttf_parser::Face<'a>) -> Self {
        Self {
            face,
            commands: Vec::new(),
            drawing: Drawing::new(),
            has_paint: false,
            transforms: vec![Self::identity_transform()],
        }
    }

    fn into_drawing(self) -> Option<Drawing> {
        self.has_paint.then_some(self.drawing)
    }

    fn identity_transform() -> ttf_parser::Transform {
        ttf_parser::Transform::new(1.0, 0.0, 0.0, 1.0, 0.0, 0.0)
    }

    fn current_transform(&self) -> ttf_parser::Transform {
        self.transforms
            .last()
            .copied()
            .unwrap_or_else(Self::identity_transform)
    }

    fn color_from_paint(paint: ttf_parser::colr::Paint<'a>) -> Option<ttf_parser::RgbaColor> {
        match paint {
            ttf_parser::colr::Paint::Solid(color) => Some(color),
            ttf_parser::colr::Paint::LinearGradient(gradient) => gradient
                .stops(0, &[])
                .find_map(|stop| (stop.color.alpha > 0).then_some(stop.color)),
            ttf_parser::colr::Paint::RadialGradient(gradient) => gradient
                .stops(0, &[])
                .find_map(|stop| (stop.color.alpha > 0).then_some(stop.color)),
            ttf_parser::colr::Paint::SweepGradient(gradient) => gradient
                .stops(0, &[])
                .find_map(|stop| (stop.color.alpha > 0).then_some(stop.color)),
        }
    }
}

impl<'a> ttf_parser::colr::Painter<'a> for ColorGlyphPainter<'a, '_> {
    fn outline_glyph(&mut self, glyph_id: ttf_parser::GlyphId) {
        self.commands.clear();
        let transform = self.current_transform();
        let mut builder = GlyphToCommands {
            commands: &mut self.commands,
            contour_start: None,
            cursor: Point::ZERO,
            transform,
        };
        self.face.outline_glyph(glyph_id, &mut builder);
    }

    fn paint(&mut self, paint: ttf_parser::colr::Paint<'a>) {
        let Some(color) = Self::color_from_paint(paint) else {
            return;
        };

        if color.alpha == 0 || self.commands.is_empty() {
            return;
        }

        self.drawing.new_fill(
            Some(FillStyle::Color(Color {
                r: color.red,
                g: color.green,
                b: color.blue,
                a: color.alpha,
            })),
            Some(FillRule::NonZero),
        );
        for command in self.commands.iter().cloned() {
            self.drawing.draw_command(command);
        }
        self.has_paint = true;
    }

    fn push_clip(&mut self) {}

    fn push_clip_box(&mut self, _clipbox: ttf_parser::colr::ClipBox) {}

    fn pop_clip(&mut self) {}

    fn push_layer(&mut self, _mode: ttf_parser::colr::CompositeMode) {}

    fn pop_layer(&mut self) {}

    fn push_transform(&mut self, transform: ttf_parser::Transform) {
        let current = self.current_transform();
        self.transforms
            .push(ttf_parser::Transform::combine(current, transform));
    }

    fn pop_transform(&mut self) {
        if self.transforms.len() > 1 {
            self.transforms.pop();
        }
    }
}

pub struct FontFileData(Arc<dyn AsRef<[u8]>>);

impl FontFileData {
    pub fn new(data: impl AsRef<[u8]> + 'static) -> Self {
        Self(Arc::new(data))
    }

    pub fn new_shared(data: Arc<dyn AsRef<[u8]>>) -> Self {
        Self(data)
    }
}

impl std::ops::Deref for FontFileData {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.0.as_ref().as_ref()
    }
}

impl std::fmt::Debug for FontFileData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FontFileData").field(&"<data>").finish()
    }
}

/// Represents a raw font file (ie .ttf).
///
/// This should be shared and reused where possible, and it's reparsed every
/// time a new glyph is required.
///
/// Parsing of a font is near-free (according to [ttf_parser::Face::parse]),
/// but the storage isn't.
///
/// Font files may contain multiple individual font faces, but those font faces
/// may reuse the same Glyph from the same file. For this reason, glyphs are
/// reused where possible.
#[derive(Debug)]
pub struct FontFace {
    data: FontFileData,
    glyphs: Vec<OnceCell<Option<Glyph>>>,
    font_index: u32,

    ascender: i32,
    descender: i32,
    leading: i16,
    scale: f32,
    might_have_kerning: bool,
}

impl FontFace {
    pub fn new(data: FontFileData, font_index: u32) -> Result<Self, ttf_parser::FaceParsingError> {
        // TODO: Support font collections

        // We validate that the font is good here, so we can just `.expect()` it later
        let face = ttf_parser::Face::parse(&data, font_index)?;

        let ascender = face.ascender() as i32;
        let descender = -face.descender() as i32;
        let leading = face.line_gap();
        let scale = face.units_per_em() as f32;
        let glyphs = vec![OnceCell::new(); face.number_of_glyphs() as usize];

        // [NA] TODO: This is technically correct for just Kerning, but in practice kerning comes in many forms.
        // We need to support GPOS to do better at this, but that's a bigger change to font rendering as a whole.
        let might_have_kerning = face
            .tables()
            .kern
            .map(|k| {
                k.subtables
                    .into_iter()
                    .any(|sub| sub.horizontal && !sub.has_state_machine)
            })
            .unwrap_or_default();

        Ok(Self {
            data,
            font_index,
            glyphs,
            ascender,
            descender,
            leading,
            scale,
            might_have_kerning,
        })
    }

    pub fn font_index(&self) -> u32 {
        self.font_index
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn metrics(&self) -> FontMetrics {
        FontMetrics {
            scale: self.scale,
            ascent: self.ascender,
            descent: self.descender,
            leading: self.leading,
        }
    }

    pub fn get_glyph(&self, character: char) -> Option<&Glyph> {
        let face = ttf_parser::Face::parse(&self.data, self.font_index)
            .expect("Font was already checked to be valid");
        if let Some(glyph_id) = face.glyph_index(character) {
            return self.glyphs[glyph_id.0 as usize]
                .get_or_init(|| {
                    if let Some(glyph) = self.get_raster_glyph(&face, glyph_id, character) {
                        return Some(glyph);
                    }

                    if face.is_color_glyph(glyph_id) {
                        let mut painter = ColorGlyphPainter::new(&face);
                        if face
                            .paint_color_glyph(
                                glyph_id,
                                0,
                                ttf_parser::RgbaColor::new(255, 255, 255, 255),
                                &mut painter,
                            )
                            .is_some()
                            && let Some(drawing) = painter.into_drawing()
                        {
                            let advance = face.glyph_hor_advance(glyph_id).map_or_else(
                                || drawing.self_bounds(true).width(),
                                |a| Twips::new(a as i32),
                            );
                            return Some(Glyph::from_drawing_with_native_color(
                                character, advance, drawing, true,
                            ));
                        }
                    }

                    let mut drawing = Drawing::new();
                    // TTF uses NonZero
                    drawing.new_fill(
                        Some(FillStyle::Color(Color::WHITE)),
                        Some(FillRule::NonZero),
                    );
                    if face
                        .outline_glyph(glyph_id, &mut GlyphToDrawing(&mut drawing))
                        .is_some()
                    {
                        let advance = face.glyph_hor_advance(glyph_id).map_or_else(
                            || drawing.self_bounds(true).width(),
                            |a| Twips::new(a as i32),
                        );
                        Some(Glyph::from_drawing(character, advance, drawing))
                    } else {
                        let advance = Twips::new(face.glyph_hor_advance(glyph_id)? as i32);
                        // If we have advance, then this is either an image, SVG or simply missing (ie whitespace)
                        Some(Glyph::whitespace(character, advance))
                    }
                })
                .as_ref();
        }
        None
    }

    fn get_raster_glyph(
        &self,
        face: &ttf_parser::Face<'_>,
        glyph_id: ttf_parser::GlyphId,
        character: char,
    ) -> Option<Glyph> {
        const RASTER_GLYPH_PPEM: u16 = 64;

        let image = face.glyph_raster_image(glyph_id, RASTER_GLYPH_PPEM)?;
        let bitmap = match image.format {
            ttf_parser::RasterImageFormat::PNG => {
                ruffle_render::utils::decode_define_bits_jpeg(image.data, None).ok()?
            }
            ttf_parser::RasterImageFormat::BitmapPremulBgra32 => {
                let expected_len = usize::from(image.width)
                    .checked_mul(usize::from(image.height))?
                    .checked_mul(4)?;
                if image.data.len() != expected_len {
                    return None;
                }

                let mut data = Vec::with_capacity(expected_len);
                for pixel in image.data.chunks_exact(4) {
                    data.extend([pixel[2], pixel[1], pixel[0], pixel[3]]);
                }
                Bitmap::new(
                    u32::from(image.width),
                    u32::from(image.height),
                    BitmapFormat::Rgba,
                    data,
                )
            }
            _ => return None,
        };

        let units_per_pixel = self.scale / f32::from(image.pixels_per_em);
        let tx = Twips::new((f32::from(image.x) * units_per_pixel) as i32);
        let ty = Twips::new(
            (self.ascender as f32
                - ((f32::from(image.y) + f32::from(image.height)) * units_per_pixel))
                as i32,
        );
        let advance = face
            .glyph_hor_advance(glyph_id)
            .map(|advance| Twips::new(i32::from(advance)))
            .unwrap_or_else(|| Twips::new((f32::from(image.width) * units_per_pixel) as i32));

        Some(Glyph::from_bitmap_with_transform_and_native_color(
            character,
            bitmap,
            advance,
            tx,
            ty,
            units_per_pixel,
            true,
        ))
    }

    pub fn has_kerning_info(&self) -> bool {
        self.might_have_kerning
    }

    pub fn get_kerning_offset(&self, left: char, right: char) -> Twips {
        let face = ttf_parser::Face::parse(&self.data, self.font_index)
            .expect("Font was already checked to be valid");

        if let Some(kern) = face.tables().kern
            && let (Some(left_glyph), Some(right_glyph)) =
                (face.glyph_index(left), face.glyph_index(right))
        {
            for subtable in kern.subtables {
                if subtable.horizontal
                    && let Some(value) = subtable.glyphs_kerning(left_glyph, right_glyph)
                {
                    return Twips::new(value as i32);
                }
            }
        }

        Twips::ZERO
    }
}
