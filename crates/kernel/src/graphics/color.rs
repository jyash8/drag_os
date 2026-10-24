#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const BLACK: Self = Self::new(0, 0, 0);
    pub const WHITE: Self = Self::new(255, 255, 255);
    pub const RED: Self = Self::new(255, 0, 0);
    pub const GREEN: Self = Self::new(0, 255, 0);
    pub const BLUE: Self = Self::new(0, 0, 255);
    pub const YELLOW: Self = Self::new(255, 255, 0);
    pub const CYAN: Self = Self::new(0, 255, 255);
    pub const MAGENTA: Self = Self::new(255, 0, 255);

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn as_u32_bgr(&self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }

    /// Scale the color intensity by a coverage byte (0–255).
    /// Used for anti-aliased font rendering.
    pub fn with_alpha(&self, alpha: u8) -> Self {
        let a = alpha as u16;
        Self {
            r: ((self.r as u16 * a) / 255) as u8,
            g: ((self.g as u16 * a) / 255) as u8,
            b: ((self.b as u16 * a) / 255) as u8,
        }
    }

    /// Blend foreground color with background color according to alpha (0..255).
    pub fn blend(&self, bg: Self, alpha: u8) -> Self {
        let a = alpha as u16;
        let inv_a = 255 - a;
        Self {
            r: ((self.r as u16 * a + bg.r as u16 * inv_a) / 255) as u8,
            g: ((self.g as u16 * a + bg.g as u16 * inv_a) / 255) as u8,
            b: ((self.b as u16 * a + bg.b as u16 * inv_a) / 255) as u8,
        }
    }

    // -- Standard ANSI 8-color palette (normal intensity) --

    pub const ANSI_BLACK: Self = Self::new(0, 0, 0);
    pub const ANSI_RED: Self = Self::new(210, 40, 40);
    pub const ANSI_GREEN: Self = Self::new(40, 210, 40);
    pub const ANSI_YELLOW: Self = Self::new(210, 160, 40);
    pub const ANSI_BLUE: Self = Self::new(60, 120, 220);
    pub const ANSI_MAGENTA: Self = Self::new(210, 40, 210);
    pub const ANSI_CYAN: Self = Self::new(40, 210, 210);
    pub const ANSI_WHITE: Self = Self::new(235, 235, 235);

    // -- Bright ANSI 8-color palette --

    pub const ANSI_BRIGHT_BLACK: Self = Self::new(85, 85, 85);
    pub const ANSI_BRIGHT_RED: Self = Self::new(255, 85, 85);
    pub const ANSI_BRIGHT_GREEN: Self = Self::new(85, 255, 85);
    pub const ANSI_BRIGHT_YELLOW: Self = Self::new(255, 255, 85);
    pub const ANSI_BRIGHT_BLUE: Self = Self::new(85, 85, 255);
    pub const ANSI_BRIGHT_MAGENTA: Self = Self::new(255, 85, 255);
    pub const ANSI_BRIGHT_CYAN: Self = Self::new(85, 255, 255);
    pub const ANSI_BRIGHT_WHITE: Self = Self::new(255, 255, 255);

    /// Look up a standard ANSI color by index (0–7).
    pub fn ansi_normal(idx: usize) -> Self {
        match idx {
            0 => Self::ANSI_BLACK,
            1 => Self::ANSI_RED,
            2 => Self::ANSI_GREEN,
            3 => Self::ANSI_YELLOW,
            4 => Self::ANSI_BLUE,
            5 => Self::ANSI_MAGENTA,
            6 => Self::ANSI_CYAN,
            _ => Self::ANSI_WHITE,
        }
    }

    /// Look up a bright ANSI color by index (0–7).
    pub fn ansi_bright(idx: usize) -> Self {
        match idx {
            0 => Self::ANSI_BRIGHT_BLACK,
            1 => Self::ANSI_BRIGHT_RED,
            2 => Self::ANSI_BRIGHT_GREEN,
            3 => Self::ANSI_BRIGHT_YELLOW,
            4 => Self::ANSI_BRIGHT_BLUE,
            5 => Self::ANSI_BRIGHT_MAGENTA,
            6 => Self::ANSI_BRIGHT_CYAN,
            _ => Self::ANSI_BRIGHT_WHITE,
        }
    }

    /// Convert a 256-color index to RGB.
    /// 0–7: standard, 8–15: bright, 16–231: 6×6×6 cube, 232–255: grayscale.
    pub fn from_ansi256(idx: u8) -> Self {
        match idx {
            0..=7 => Self::ansi_normal(idx as usize),
            8..=15 => Self::ansi_bright((idx - 8) as usize),
            16..=231 => {
                let idx = idx - 16;
                let r = (idx / 36) % 6;
                let g = (idx / 6) % 6;
                let b = idx % 6;
                let to_val = |c: u8| if c == 0 { 0 } else { 55 + 40 * c };
                Self::new(to_val(r), to_val(g), to_val(b))
            }
            232..=255 => {
                let v = 8 + 10 * (idx - 232);
                Self::new(v, v, v)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn as_u32_bgr_layout() {
        let c = Color::RED; // r=255,g=0,b=0
        assert_eq!(c.as_u32_bgr(), 0x00FF_0000, "r in high byte for BGR");
        let c = Color::BLUE; // r=0,g=0,b=255
        assert_eq!(c.as_u32_bgr(), 0x0000_00FF, "b in low byte for BGR");
    }

    #[test_case]
    fn with_alpha_linear() {
        let c = Color::WHITE; // 255,255,255
        assert_eq!(c.with_alpha(0), Color::BLACK);
        assert_eq!(c.with_alpha(255), Color::WHITE);
        assert_eq!(c.with_alpha(128), Color::new(128, 128, 128));
    }

    #[test_case]
    fn blend_matches_alpha() {
        let fg = Color::WHITE;
        let bg = Color::BLACK;
        assert_eq!(fg.blend(bg, 255), Color::WHITE);
        assert_eq!(fg.blend(bg, 0), Color::BLACK);
        assert_eq!(fg.blend(bg, 128), Color::new(128, 128, 128));
    }

    #[test_case]
    fn blend_componentwise() {
        let fg = Color::new(200, 100, 0);
        let bg = Color::new(0, 0, 100);
        // alpha=128: (fg*a + bg*inv_a)/255 with a=128, inv_a=127
        // r=(200*128+0)/255, g=(100*128+0)/255, b=(0*128+100*127)/255
        assert_eq!(fg.blend(bg, 128), Color::new(100, 50, 49));
    }

    #[test_case]
    fn ansi256_uniform() {
        // Grayscale tail: 233 -> 18,18,18
        assert_eq!(Color::from_ansi256(233), Color::new(18, 18, 18));
        // Cube: idx 16 -> (0,0,0); 21 -> 5,0,0? idx16-16=0 -> r0 g0 b0
        assert_eq!(Color::from_ansi256(16), Color::new(0, 0, 0));
        // 0 -> standard black, 7 -> white, 8 -> bright black
        assert_eq!(Color::from_ansi256(0), Color::ANSI_BLACK);
        assert_eq!(Color::from_ansi256(7), Color::ANSI_WHITE);
        assert_eq!(Color::from_ansi256(8), Color::ANSI_BRIGHT_BLACK);
    }
}
