//! Mercury's logo: the quicksilver orb, rasterised from vector shapes (no assets), or any image
//! the user picks. Custom images are copied into Mercury's data folder as a square PNG so the logo
//! keeps working if the original file moves.

use crate::config::Logo;

type Rgba = [u8; 4];

#[derive(Debug, Clone, Copy)]
pub struct LogoColors {
    pub badge: Rgba,
    pub accent: Rgba,
    pub ring: Rgba,
}

pub const DARK: LogoColors = LogoColors { badge: [0x1c, 0x2a, 0x33, 255], accent: [0x5e, 0xc8, 0xd8, 255], ring: [0x8a, 0x94, 0xa3, 255] };

pub const LIGHT: LogoColors = LogoColors { badge: [0xd7, 0xf0, 0xf3, 255], accent: [0x0b, 0x86, 0x9a, 255], ring: [0x5f, 0x69, 0x78, 255] };

#[derive(Debug, Clone)]
enum Shape {
    Circle(f32, f32, f32),
    /// Annulus between r0 and r1.
    Ring(f32, f32, f32, f32),
}

impl Shape {
    fn contains(&self, x: f32, y: f32) -> bool {
        match self {
            Shape::Circle(cx, cy, r) => (x - cx).powi(2) + (y - cy).powi(2) <= r * r,
            Shape::Ring(cx, cy, r0, r1) => {
                let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
                d >= *r0 && d <= *r1
            }
        }
    }
}

fn orb_layers(c: &LogoColors) -> Vec<(Shape, Rgba)> {
    vec![
        (Shape::Ring(50.0, 50.0, 31.0, 35.0), c.ring),
        (Shape::Circle(50.0, 50.0, 23.0), c.accent),
        (Shape::Circle(42.5, 42.5, 6.5), [0xe6, 0xf8, 0xfa, 235]),
    ]
}

fn blend(dst: &mut [f32; 4], src: Rgba) {
    let a = src[3] as f32 / 255.0;
    for i in 0..3 {
        dst[i] = dst[i] * (1.0 - a) + src[i] as f32 * a;
    }
    dst[3] = dst[3] + a * (1.0 - dst[3]);
}

/// Render the orb at `size`×`size` RGBA. With `badge`, it sits on a filled circle.
pub fn render_orb(size: u32, colors: &LogoColors, badge: bool) -> Vec<u8> {
    let layers = orb_layers(colors);
    let clip = Shape::Circle(50.0, 50.0, 48.0);
    const SS: u32 = 4;
    let mut out = vec![0u8; (size * size * 4) as usize];
    let scale = 100.0 / size as f32;
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) * scale;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) * scale;
                    if !clip.contains(x, y) {
                        continue;
                    }
                    let mut px_col = [0f32; 4];
                    if badge {
                        blend(&mut px_col, colors.badge);
                    }
                    for (shape, col) in &layers {
                        if shape.contains(x, y) {
                            blend(&mut px_col, *col);
                        }
                    }
                    // Premultiply for averaging.
                    for i in 0..3 {
                        acc[i] += px_col[i] * px_col[3];
                    }
                    acc[3] += px_col[3];
                }
            }
            let n = (SS * SS) as f32;
            let a = acc[3] / n;
            let i = ((py * size + px) * 4) as usize;
            if a > 0.0 {
                for c in 0..3 {
                    out[i + c] = (acc[c] / acc[3]).round().clamp(0.0, 255.0) as u8;
                }
                out[i + 3] = (a * 255.0).round() as u8;
            }
        }
    }
    out
}

/// Decode `src`, fit it into a transparent 256×256 square (aspect kept) and save it as PNG in
/// `data_dir`. Returns the saved path.
pub fn import_custom(src: &std::path::Path, data_dir: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context;
    let img = image::ImageReader::open(src)
        .with_context(|| format!("opening {}", src.display()))?
        .with_guessed_format()?
        .decode()
        .with_context(|| format!("{} isn't an image Mercury can read (PNG, JPEG, WebP, GIF, BMP, ICO)", src.display()))?;
    let square = fit_square(&img, 256);
    std::fs::create_dir_all(data_dir)?;
    let dst = data_dir.join("custom-logo.png");
    square.save_with_format(&dst, image::ImageFormat::Png)?;
    Ok(dst)
}

fn fit_square(img: &image::DynamicImage, size: u32) -> image::RgbaImage {
    let scaled = img.resize(size, size, image::imageops::FilterType::Lanczos3).to_rgba8();
    let mut canvas = image::RgbaImage::new(size, size);
    let x = (size - scaled.width()) / 2;
    let y = (size - scaled.height()) / 2;
    image::imageops::overlay(&mut canvas, &scaled, x as i64, y as i64);
    canvas
}

/// Load a saved custom logo as `size`×`size` RGBA.
pub fn load_custom(path: &std::path::Path, size: u32) -> anyhow::Result<Vec<u8>> {
    let img = image::open(path)?;
    Ok(fit_square(&img, size).into_raw())
}

/// RGBA pixels for the configured logo at `size` (falls back to the orb if the image is unusable).
pub fn pixels(ui: &crate::config::UiConfig, size: u32, dark: bool) -> Vec<u8> {
    if ui.logo == Logo::Custom
        && let Some(p) = &ui.custom_logo
        && let Ok(px) = load_custom(p, size)
    {
        return px;
    }
    render_orb(size, if dark { &DARK } else { &LIGHT }, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(img: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [img[i], img[i + 1], img[i + 2], img[i + 3]]
    }

    #[test]
    fn custom_logo_import_and_fallback() {
        let d = tempfile::tempdir().unwrap();
        // A wide 40×20 red JPEG-free PNG: it should be centred in a transparent square.
        let src = d.path().join("mine.png");
        image::RgbaImage::from_pixel(40, 20, image::Rgba([255, 0, 0, 255])).save(&src).unwrap();
        let saved = import_custom(&src, &d.path().join("data")).unwrap();
        assert!(saved.ends_with("custom-logo.png"));
        let px = load_custom(&saved, 64).unwrap();
        assert_eq!(px.len(), 64 * 64 * 4);
        assert_eq!(&px[(32 * 64 + 32) * 4..][..4], &[255, 0, 0, 255], "centre is the image");
        assert_eq!(px[3], 0, "top-left corner is transparent padding");
        // The original can move; Mercury keeps its own copy.
        std::fs::remove_file(&src).unwrap();
        let ui = crate::config::UiConfig { logo: Logo::Custom, custom_logo: Some(saved), ..Default::default() };
        assert_eq!(&pixels(&ui, 64, true)[(32 * 64 + 32) * 4..][..4], &[255, 0, 0, 255]);
        // Missing or broken files fall back to the orb.
        let broken = crate::config::UiConfig { logo: Logo::Custom, custom_logo: Some(d.path().join("gone.png")), ..Default::default() };
        assert_eq!(pixels(&broken, 64, true), render_orb(64, &DARK, false));
        std::fs::write(d.path().join("notes.txt"), "hello").unwrap();
        assert!(import_custom(&d.path().join("notes.txt"), d.path()).unwrap_err().to_string().contains("isn't an image"));
    }

    #[test]
    fn orb_and_light_theme() {
        let img = render_orb(64, &LIGHT, false);
        assert_eq!(px(&img, 64, 0, 0)[3], 0);
        assert_eq!(px(&img, 64, 36, 36), LIGHT.accent);
    }
}
