use fontdue::Font;
use std::fs;

fn main() {
    let font_bytes = fs::read("NotoSansMNerdFontMono-Regular.ttf").unwrap();
    let font = Font::from_bytes(font_bytes, fontdue::FontSettings::default()).unwrap();
    
    let (metrics_A, _) = font.rasterize('A', 16.0);
    println!("A: {:?}", metrics_A);
    let (metrics_a, _) = font.rasterize('a', 16.0);
    println!("a: {:?}", metrics_a);
    
    let font_metrics = font.horizontal_line_metrics(16.0);
    println!("Line metrics: {:?}", font_metrics);
}
