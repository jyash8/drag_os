#![no_std]

pub struct RasterizedChar {
    pub width: usize,
    pub height: usize,
    pub raster: &'static [u8],
}

impl RasterizedChar {
    pub fn width(&self) -> usize { self.width }
    pub fn height(&self) -> usize { self.height }
    pub fn raster(&self) -> RasterRows {
        RasterRows { raster: self.raster, width: self.width }
    }
}

pub struct RasterRows {
    raster: &'static [u8],
    width: usize,
}
impl RasterRows {
    pub fn iter(&self) -> RasterRowIter {
        RasterRowIter { raster: self.raster, width: self.width, pos: 0 }
    }
}

pub struct RasterRowIter {
    raster: &'static [u8],
    width: usize,
    pos: usize,
}
impl core::iter::Iterator for RasterRowIter {
    type Item = &'static [u8];
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.raster.len() {
            None
        } else {
            let row = &self.raster[self.pos .. self.pos + self.width];
            self.pos += self.width;
            Some(row)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RasterHeight {
    Size16 = 16,
    Size20 = 20,
    Size32 = 32,
}

impl RasterHeight {
    pub fn val(&self) -> usize {
        *self as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontWeight {
    Regular,
}

include!(concat!(env!("OUT_DIR"), "/generated.rs"));

pub fn get_raster(c: char, _weight: FontWeight, height: RasterHeight) -> Option<RasterizedChar> {
    match height {
        RasterHeight::Size16 => get_raster_16(c),
        RasterHeight::Size20 => get_raster_20(c),
        RasterHeight::Size32 => get_raster_32(c),
    }
}

pub fn get_raster_width(_weight: FontWeight, height: RasterHeight) -> usize {
    match height {
        RasterHeight::Size16 => 10,
        RasterHeight::Size20 => 12,
        RasterHeight::Size32 => 19,
    }
}
