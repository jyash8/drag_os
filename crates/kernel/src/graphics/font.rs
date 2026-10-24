/// Runtime-switchable font support.
///
/// Provides a `FontKind` enum that the terminal can use to select between
/// the built-in 8×8 bitmap font and several Noto Sans Mono raster sizes.

use nerdfont_bitmap::{FontWeight, RasterHeight, RasterizedChar, get_raster, get_raster_width};

/// The set of fonts available at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontKind {
    /// Classic 8×8 bitmap font (font8x8 crate). Tiny but fast.
    Bitmap8x8,
    /// Noto Sans Mono, 16px height.
    Noto16,
    /// Noto Sans Mono, 20px height.
    Noto20,
    /// Noto Sans Mono, 32px height.
    Noto32,
}

impl FontKind {
    /// Character width in pixels (unscaled).
    pub fn char_width(&self) -> usize {
        match self {
            FontKind::Bitmap8x8 => 8,
            FontKind::Noto16 => get_raster_width(FontWeight::Regular, RasterHeight::Size16),
            FontKind::Noto20 => get_raster_width(FontWeight::Regular, RasterHeight::Size20),
            FontKind::Noto32 => get_raster_width(FontWeight::Regular, RasterHeight::Size32),
        }
    }

    /// Character height in pixels (unscaled).
    pub fn char_height(&self) -> usize {
        match self {
            FontKind::Bitmap8x8 => 8,
            FontKind::Noto16 => RasterHeight::Size16.val(),
            FontKind::Noto20 => RasterHeight::Size20.val(),
            FontKind::Noto32 => RasterHeight::Size32.val(),
        }
    }

    /// Line height in pixels (unscaled) — includes some spacing.
    pub fn line_height(&self) -> usize {
        match self {
            FontKind::Bitmap8x8 => 10,
            _ => self.char_height() + 2,
        }
    }

    /// Cycle to the next font variant.
    pub fn next(&self) -> Self {
        match self {
            FontKind::Bitmap8x8 => FontKind::Noto16,
            FontKind::Noto16 => FontKind::Noto20,
            FontKind::Noto20 => FontKind::Noto32,
            FontKind::Noto32 => FontKind::Bitmap8x8,
        }
    }

    /// Human-readable name for display.
    pub fn name(&self) -> &'static str {
        match self {
            FontKind::Bitmap8x8 => "Bitmap 8x8",
            FontKind::Noto16 => "Noto Sans Mono 16",
            FontKind::Noto20 => "Noto Sans Mono 20",
            FontKind::Noto32 => "Noto Sans Mono 32",
        }
    }
}

/// Rasterise a character with the Noto Sans Mono font at the given height.
/// Returns `None` if the character has no glyph (falls back to '?' internally).
pub fn noto_raster(ch: char, height: RasterHeight) -> Option<RasterizedChar> {
    get_raster(ch, FontWeight::Regular, height)
        .or_else(|| get_raster('?', FontWeight::Regular, height))
}
