use fontdue::Font;
use std::fs;
use std::io::Write;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=NotoSansMNerdFontMono-Regular.ttf");
    let font_bytes = fs::read("NotoSansMNerdFontMono-Regular.ttf").unwrap();
    let font = Font::from_bytes(font_bytes, fontdue::FontSettings::default()).unwrap();
    
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("generated.rs");
    let mut f = fs::File::create(&dest_path).unwrap();
    
    let sizes = [16, 20, 32];
    
    let mut chars_to_render = Vec::new();
    for &c in font.chars().keys() {
        let u = c as u32;
        if (32..=126).contains(&u) || (0xe000..=0xf8ff).contains(&u) || (0xf0000..=0xf1000).contains(&u) {
            chars_to_render.push(c);
        }
    }
    chars_to_render.sort();
    
    for &size in &sizes {
        let metrics_a = font.metrics('A', size as f32);
        let cell_width = metrics_a.advance_width.round() as usize;
        let cell_height = size;
        let baseline = (size as f32 * 0.8).round() as i32;
        
        let mut raster_bin = fs::File::create(Path::new(&out_dir).join(format!("raster_{}.bin", size))).unwrap();
        
        writeln!(f, "pub static CHARS_{}: &[char] = &[", size).unwrap();
        for &c in &chars_to_render {
            let u = c as u32;
            writeln!(f, "    '\\u{{{:x}}}',", u).unwrap();
            
            let (metrics, bitmap) = font.rasterize(c, size as f32);
            let mut cell = vec![0u8; cell_width * cell_height];
            
            let start_x = metrics.xmin;
            let start_y = baseline - metrics.ymin - metrics.height as i32;
            
            for y in 0..metrics.height {
                for x in 0..metrics.width {
                    let cx = start_x + x as i32;
                    let cy = start_y + y as i32;
                    
                    if cx >= 0 && cx < cell_width as i32 && cy >= 0 && cy < cell_height as i32 {
                        cell[cy as usize * cell_width + cx as usize] = bitmap[y * metrics.width + x];
                    }
                }
            }
            raster_bin.write_all(&cell).unwrap();
        }
        writeln!(f, "];").unwrap();
        writeln!(f, "pub static RASTER_{}: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/raster_{}.bin\"));", size, size).unwrap();
        
        writeln!(f, "pub fn get_raster_{}(c: char) -> Option<RasterizedChar> {{", size).unwrap();
        writeln!(f, "    CHARS_{}.binary_search(&c).ok().map(|idx| RasterizedChar {{", size).unwrap();
        writeln!(f, "        width: {},", cell_width).unwrap();
        writeln!(f, "        height: {},", cell_height).unwrap();
        writeln!(f, "        raster: &RASTER_{}[idx * {} .. (idx + 1) * {}],", size, cell_width * cell_height, cell_width * cell_height).unwrap();
        writeln!(f, "    }})").unwrap();
        writeln!(f, "}}").unwrap();
    }
}
