//! Decoding images with macOS's ImageIO (PNG, JPEG, GIF, TIFF, BMP, HEIC, WebP, ICO...), through
//! our own CoreFoundation / CoreGraphics / ImageIO declarations: the file becomes a
//! `CGImage`, which is drawn into an RGBA bitmap.

use std::ffi::c_void;
use std::path::Path;

type CFTypeRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataCreate(allocator: CFTypeRef, bytes: *const u8, length: isize) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    fn CGImageSourceCreateWithData(data: CFTypeRef, options: CFTypeRef) -> CFTypeRef;
    fn CGImageSourceCreateImageAtIndex(source: CFTypeRef, index: usize, options: CFTypeRef) -> CFTypeRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGImageGetWidth(image: CFTypeRef) -> usize;
    fn CGImageGetHeight(image: CFTypeRef) -> usize;
    fn CGColorSpaceCreateDeviceRGB() -> CFTypeRef;
    fn CGBitmapContextCreate(data: *mut c_void, width: usize, height: usize, bits: usize, row_bytes: usize, space: CFTypeRef, info: u32) -> CFTypeRef;
    fn CGContextDrawImage(ctx: CFTypeRef, rect: CGRect, image: CFTypeRef);
}

/// kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big: R, G, B, A bytes.
const RGBA_PREMULTIPLIED: u32 = 1 | (4 << 12);
/// Larger images are refused (a 16K × 16K RGBA bitmap is already 1 GB).
const MAX_PIXELS: usize = 100_000_000;

/// A decoded image, and its size in pixels.
pub fn decode(bytes: &[u8]) -> Result<render::Image, String> {
    // SAFETY: every object created here is released before returning; the bitmap buffer
    // outlives the context drawing into it.
    unsafe {
        let data = CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize);
        if data.is_null() {
            return Err("Couldn't read the image.".into());
        }
        let source = CGImageSourceCreateWithData(data, std::ptr::null());
        CFRelease(data);
        if source.is_null() {
            return Err("The file isn't an image macOS can read.".into());
        }
        let image = CGImageSourceCreateImageAtIndex(source, 0, std::ptr::null());
        CFRelease(source);
        if image.is_null() {
            return Err("The file isn't an image macOS can read.".into());
        }
        let (w, h) = (CGImageGetWidth(image), CGImageGetHeight(image));
        if w == 0 || h == 0 || w.saturating_mul(h) > MAX_PIXELS {
            CFRelease(image);
            return Err(format!("The image is too large to show ({w} × {h})."));
        }
        let mut rgba = vec![0u8; w * h * 4];
        let space = CGColorSpaceCreateDeviceRGB();
        let ctx = CGBitmapContextCreate(rgba.as_mut_ptr().cast(), w, h, 8, w * 4, space, RGBA_PREMULTIPLIED);
        CFRelease(space);
        if ctx.is_null() {
            CFRelease(image);
            return Err("Couldn't decode the image.".into());
        }
        CGContextDrawImage(ctx, CGRect { x: 0.0, y: 0.0, w: w as f64, h: h as f64 }, image);
        CFRelease(ctx);
        CFRelease(image);
        // Back to straight alpha, which the renderer blends.
        for px in rgba.chunks_exact_mut(4) {
            let a = px[3] as u32;
            if a > 0 && a < 255 {
                for c in &mut px[..3] {
                    *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        Ok(render::Image::new(w as u32, h as u32, rgba))
    }
}

/// Whether `path` is an image the preview shows (by extension, like the image preview).
pub fn is_image(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "bmp" | "tif" | "tiff" | "webp" | "ico" | "heic" | "jfif")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2×1 PNG: an opaque red pixel and a half-transparent blue one.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
        0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0xF4, 0x22, 0x7F,
        0x8A, 0x00, 0x00, 0x00, 0x0E, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
        0x42, 0x0D, 0x00, 0x0F, 0x7A, 0x03, 0x7E, 0x77, 0xE9, 0x7F, 0x97, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn decodes_a_png() {
        let img = decode(PNG).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(&img.rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(img.rgba[7], 128);
        assert!(img.rgba[6] > 240, "{:?}", &img.rgba[4..8]);
        assert!(decode(b"not an image").is_err());
        assert!(is_image(Path::new("a/B.PNG")) && !is_image(Path::new("a.rs")));
    }
}
