use font8x8::UnicodeFonts;
use nerdfont_bitmap::RasterHeight;
use shared::boot::boot_info::{FramebufferInfo, HHDM_BASE};
use shared::memory::address::VirtAddr;

use crate::graphics::color::Color;
use crate::graphics::font::{FontKind, noto_raster};

pub mod color;
pub mod font;
pub mod terminal;
pub mod vt;

pub static mut FRAMEBUFFER_INFO: Option<FramebufferInfo> = None;

pub struct Display {
    info: FramebufferInfo,
    buffer: &'static mut [u32],
}

impl Display {
    pub fn new(info: FramebufferInfo) -> Self {
        // Expose the GOP framebuffer info globally so DRM blit_dumb_to_gop can
        // find the scanout target without needing a separate init step.
        // SAFETY: FRAMEBUFFER_INFO is only written once here (at boot) and read
        // from drivers/drm afterward; no concurrent writers.
        crate::dprintln!("[GOP] mode: {}x{} stride={} fmt={:?}", info.width, info.height, info.stride, info.format);

        unsafe { crate::graphics::FRAMEBUFFER_INFO = Some(info); }
        let buffer = unsafe {
            core::slice::from_raw_parts_mut(
                VirtAddr(HHDM_BASE.0 + info.address.0).as_mut_ptr::<u32>(),
                (info.size / 4) as usize,
            )
        };

        Self { info, buffer }
    }

    pub fn width(&self) -> usize {
        self.info.width
    }

    pub fn height(&self) -> usize {
        self.info.height
    }

    /// The core drawing function. All shapes rely on this.
    #[inline(always)]
    pub fn draw_pixel(&mut self, x: usize, y: usize, color: Color) {
        // Bounds checking to prevent page faults!
        if x >= self.width() || y >= self.height() {
            return;
        }

        // The "stride" (pixels_per_scanline) is often larger than the width.
        // We MUST use the stride to calculate the Y offset, not the width!
        let stride = self.info.stride as usize;
        let index = (y * stride) + x;

        self.buffer[index] = color.as_u32_bgr();
    }

    /// Fills the entire screen with a single color
    pub fn clear(&mut self, color: Color) {
        let col_u32 = color.as_u32_bgr();
        // Since we are clearing the whole buffer, we can just fill the slice.
        self.buffer.fill(col_u32);
    }

    /// Draws a filled rectangle
    pub fn draw_rect(&mut self, x: usize, y: usize, width: usize, height: usize, color: Color) {
        for row in y..(y + height) {
            for col in x..(x + width) {
                self.draw_pixel(col, row, color);
            }
        }
    }

    /// Draws a character using the specified font.
    pub fn draw_char(
        &mut self,
        x: usize,
        y: usize,
        character: char,
        color: Color,
        bg_color: Color,
        scale: usize,
        font: FontKind,
    ) {
        match font {
            FontKind::Bitmap8x8 => self.draw_char_8x8(x, y, character, color, scale),
            FontKind::Noto16 => self.draw_char_noto(x, y, character, color, bg_color, scale, RasterHeight::Size16),
            FontKind::Noto20 => self.draw_char_noto(x, y, character, color, bg_color, scale, RasterHeight::Size20),
            FontKind::Noto32 => self.draw_char_noto(x, y, character, color, bg_color, scale, RasterHeight::Size32),
        }
    }

    /// Draws a single 8x8 character to the screen.
    fn draw_char_8x8(&mut self, x: usize, y: usize, character: char, color: Color, scale: usize) {
        if let Some(glyph) = font8x8::BASIC_FONTS.get(character) {
            for (row, byte) in glyph.iter().enumerate() {
                for col in 0..8 {
                    if (byte >> col) & 1 != 0 {
                        let scaled_x = x + (col * scale);
                        let scaled_y = y + (row * scale);
                        self.draw_rect(scaled_x, scaled_y, scale, scale, color);
                    }
                }
            }
        }
    }

    /// Draws a character using the Noto Sans Mono pre-rasterized font.
    /// Each pixel is a coverage byte (0=transparent, 255=opaque).
    fn draw_char_noto(
        &mut self,
        x: usize,
        y: usize,
        character: char,
        color: Color,
        bg_color: Color,
        scale: usize,
        height: RasterHeight,
    ) {
        if let Some(raster) = noto_raster(character, height) {
            for (row_idx, row) in raster.raster().iter().enumerate() {
                for (col_idx, &coverage) in row.iter().enumerate() {
                    if coverage > 0 {
                        // Blend the font color with the background color according to coverage alpha
                        let blended = color.blend(bg_color, coverage);
                        let px = x + col_idx * scale;
                        let py = y + row_idx * scale;
                        if scale == 1 {
                            self.draw_pixel(px, py, blended);
                        } else {
                            self.draw_rect(px, py, scale, scale, blended);
                        }
                    }
                }
            }
        }
    }

    /// Draws a full string of text to the screen
    pub fn draw_string(
        &mut self,
        x: usize,
        y: usize,
        text: &str,
        color: Color,
        bg_color: Color,
        scale: usize,
        font: FontKind,
    ) {
        let mut current_x = x;
        let mut current_y = y;

        let char_width = font.char_width() * scale;
        let line_height = font.line_height() * scale;

        for ch in text.chars() {
            if ch == '\n' {
                current_x = x;
                current_y += line_height;
                continue;
            }

            self.draw_char(current_x, current_y, ch, color, bg_color, scale, font);

            current_x += char_width;
            if current_x + char_width > self.width() {
                current_x = x;
                current_y += line_height;
            }
        }
    }

    /// Scrolls the entire screen up by a specific number of pixels,
    /// filling the newly exposed bottom area with the background color.
    pub fn scroll_up(&mut self, pixels: usize, background: Color) {
        let stride = self.info.stride as usize;
        let height = self.height();
        let offset = pixels * stride;
        let total_elemets = height * stride;

        self.buffer.copy_within(offset..total_elemets, 0);
        self.draw_rect(0, height - pixels, self.width(), pixels, background);
    }
}
