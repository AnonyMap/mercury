//! Attachments: images go to vision models as `image_url` parts (downscaled, re-encoded); text and
//! code files are inlined into the message as fenced blocks.

use std::io::Cursor;
use std::path::Path;

use base64::Engine;

/// Longest image side sent to a model. Most vision models downscale to about this anyway, and it
/// keeps a phone photo from becoming a 10 MB request.
pub const MAX_IMAGE_SIDE: u32 = 1568;
/// Largest text file inlined.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp"];

#[derive(Debug, Clone, PartialEq)]
pub enum AttachKind {
    Image { data_url: String, width: u32, height: u32 },
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    pub name: String,
    pub kind: AttachKind,
}

impl Attachment {
    pub fn is_image(&self) -> bool {
        matches!(self.kind, AttachKind::Image { .. })
    }

    /// Short label for the composer chip.
    pub fn label(&self) -> String {
        match &self.kind {
            AttachKind::Image { width, height, .. } => format!("{} ({width}x{height})", self.name),
            AttachKind::Text(t) => format!("{} ({} lines)", self.name, t.lines().count()),
        }
    }
}

/// Read a file as an attachment.
pub fn load(path: &Path) -> Result<Attachment, String> {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
    let meta = std::fs::metadata(path).map_err(|e| format!("{name}: {e}"))?;
    if meta.is_dir() {
        return Err(format!("{name} is a folder; attach files from it instead"));
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
    if IMAGE_EXTS.contains(&ext.as_str()) {
        return image(&name, &bytes);
    }
    if ext == "pdf" {
        return Err(format!("{name}: PDFs can't be attached yet; paste the text, or ask the agent to read the file"));
    }
    text(&name, &bytes)
}

pub fn image(name: &str, bytes: &[u8]) -> Result<Attachment, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("{name}: not a readable image ({e})"))?;
    let img = if img.width().max(img.height()) > MAX_IMAGE_SIDE {
        img.resize(MAX_IMAGE_SIDE, MAX_IMAGE_SIDE, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let (width, height) = (img.width(), img.height());
    let mut out = Cursor::new(Vec::new());
    // Photos as JPEG (small); anything with transparency as PNG (exact).
    let (mime, res) = if img.color().has_alpha() {
        ("image/png", img.write_to(&mut out, image::ImageFormat::Png))
    } else {
        ("image/jpeg", img.to_rgb8().write_to(&mut out, image::ImageFormat::Jpeg))
    };
    res.map_err(|e| format!("{name}: couldn't encode ({e})"))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(out.into_inner());
    Ok(Attachment { name: name.into(), kind: AttachKind::Image { data_url: format!("data:{mime};base64,{b64}"), width, height } })
}

pub fn text(name: &str, bytes: &[u8]) -> Result<Attachment, String> {
    if bytes.len() > MAX_TEXT_BYTES {
        return Err(format!(
            "{name} is {} KB; files over {} KB aren't inlined — ask the agent to read it instead",
            bytes.len() / 1024,
            MAX_TEXT_BYTES / 1024
        ));
    }
    if bytes.contains(&0) {
        return Err(format!("{name} looks like a binary file"));
    }
    let t = String::from_utf8(bytes.to_vec()).map_err(|_| format!("{name} isn't UTF-8 text"))?;
    Ok(Attachment { name: name.into(), kind: AttachKind::Text(t) })
}

/// The message text with text files appended, and the image data URLs.
pub fn compose(text: &str, atts: &[Attachment]) -> (String, Vec<String>) {
    let mut out = text.trim_end().to_string();
    let mut images = vec![];
    for a in atts {
        match &a.kind {
            AttachKind::Image { data_url, .. } => images.push(data_url.clone()),
            AttachKind::Text(t) => {
                let fence = if t.contains("```") { "````" } else { "```" };
                let lang = Path::new(&a.name).extension().and_then(|e| e.to_str()).unwrap_or("");
                out.push_str(&format!("\n\n[Attached file: {}]\n{fence}{lang}\n{}\n{fence}", a.name, t.trim_end()));
            }
        }
    }
    (out.trim_start().to_string(), images)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, alpha: bool) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        if alpha {
            image::RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 128])).write_to(&mut out, image::ImageFormat::Png).unwrap();
        } else {
            image::RgbImage::from_pixel(w, h, image::Rgb([200, 100, 50])).write_to(&mut out, image::ImageFormat::Png).unwrap();
        }
        out.into_inner()
    }

    #[test]
    fn images_are_downscaled_and_encoded() {
        let big = image("photo.png", &png(4000, 2000, false)).unwrap();
        match &big.kind {
            AttachKind::Image { data_url, width, height } => {
                assert_eq!((*width, *height), (MAX_IMAGE_SIDE, MAX_IMAGE_SIDE / 2));
                assert!(data_url.starts_with("data:image/jpeg;base64,"));
            }
            _ => panic!(),
        }
        let small = image("icon.png", &png(32, 32, true)).unwrap();
        assert!(matches!(&small.kind, AttachKind::Image { data_url, width: 32, .. } if data_url.starts_with("data:image/png;base64,")));
        assert!(image("x.png", b"nope").is_err());
    }

    #[test]
    fn text_files_and_compose() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(d.path().join("blob.bin"), [0u8, 1, 2]).unwrap();
        std::fs::write(d.path().join("doc.pdf"), "%PDF").unwrap();
        std::fs::write(d.path().join("pic.png"), png(10, 10, false)).unwrap();
        let rs = load(&d.path().join("main.rs")).unwrap();
        assert_eq!(rs.label(), "main.rs (1 lines)");
        assert!(load(&d.path().join("blob.bin")).unwrap_err().contains("binary"));
        assert!(load(&d.path().join("doc.pdf")).unwrap_err().contains("PDF"));
        assert!(load(d.path()).unwrap_err().contains("folder"));
        let pic = load(&d.path().join("pic.png")).unwrap();
        assert!(pic.is_image());
        let (t, imgs) = compose("look at these", &[rs, pic]);
        assert!(t.starts_with("look at these\n\n[Attached file: main.rs]\n```rs\nfn main() {}\n```"), "{t}");
        assert_eq!(imgs.len(), 1);
        let md = text("a.md", b"```\ncode\n```").unwrap();
        assert!(compose("", &[md]).0.contains("````md\n```\ncode\n```\n````"));
        assert!(text("big.txt", &vec![b'a'; MAX_TEXT_BYTES + 1]).unwrap_err().contains("aren't inlined"));
    }
}
