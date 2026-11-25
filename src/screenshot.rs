//! Shared validation at every screenshot boundary (CDP, HTTP, MCP and CLI).
use anyhow::{bail, Context, Result};
use image::ImageFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ValidatedImage {
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

pub(crate) fn validate_with_dimensions(bytes: &[u8]) -> Result<ValidatedImage> {
    if bytes.is_empty() {
        bail!("empty screenshot data");
    }
    let (format, mime) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        // Decoders may accept a missing end marker. Require a complete capture.
        if !bytes.ends_with(b"\0\0\0\0IEND\xae\x42\x60\x82") {
            bail!("truncated PNG screenshot (missing IEND)");
        }
        (ImageFormat::Png, "image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        if !bytes.ends_with(b"\xff\xd9") {
            bail!("truncated JPEG screenshot (missing EOI)");
        }
        (ImageFormat::Jpeg, "image/jpeg")
    } else {
        bail!(
            "screenshot data is neither PNG nor JPEG ({} bytes)",
            bytes.len()
        );
    };
    // Use the reader's default allocation limits for untrusted HTTP payloads.
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    reader.limits(image::Limits::default());
    let img = reader.decode().context("corrupted screenshot data")?;
    Ok(ValidatedImage {
        mime,
        width: img.width(),
        height: img.height(),
    })
}

#[cfg(test)]
pub(crate) fn validate(bytes: &[u8]) -> Result<&'static str> {
    validate_with_dimensions(bytes).map(|v| v.mime)
}

/// Reject HTTP errors even if their bodies happen to contain valid images.
pub(crate) fn validate_response_with_dimensions(
    status: reqwest::StatusCode,
    bytes: &[u8],
) -> Result<ValidatedImage> {
    if !status.is_success() {
        let detail: String = String::from_utf8_lossy(bytes).chars().take(500).collect();
        bail!("screenshot HTTP {status}: {detail}");
    }
    validate_with_dimensions(bytes)
}

/// Reject HTTP errors even if their bodies happen to contain valid images.
#[cfg(test)]
pub(crate) fn validate_response(status: reqwest::StatusCode, bytes: &[u8]) -> Result<&'static str> {
    validate_response_with_dimensions(status, bytes).map(|v| v.mime)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture_sized(format: ImageFormat, w: u32, h: u32) -> Vec<u8> {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            w,
            h,
            image::Rgb([1, 2, 3]),
        ));
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    fn capture(format: ImageFormat) -> Vec<u8> {
        capture_sized(format, 2, 2)
    }

    #[test]
    fn reports_exact_dimensions_for_png_and_jpeg() {
        for (format, mime) in [
            (ImageFormat::Png, "image/png"),
            (ImageFormat::Jpeg, "image/jpeg"),
        ] {
            for (w, h) in [(2, 2), (320, 180), (1170, 2532)] {
                let bytes = capture_sized(format, w, h);
                let img = validate_with_dimensions(&bytes).unwrap();
                assert_eq!(img, ValidatedImage { mime, width: w, height: h });
            }
        }
    }

    #[test]
    fn rejects_empty_error_bodies_and_garbage() {
        for bytes in [
            b"".as_slice(),
            b"{\"ok\":false,\"error\":\"dead browser\"}",
            b"<html>502 Bad Gateway</html>",
            b"screenshot timed out",
            b"garbage",
        ] {
            assert!(validate(bytes).is_err());
        }
    }

    #[test]
    fn detects_valid_png_and_jpeg_and_rejects_truncation() {
        for (format, mime) in [
            (ImageFormat::Png, "image/png"),
            (ImageFormat::Jpeg, "image/jpeg"),
        ] {
            let bytes = capture(format);
            assert_eq!(validate(&bytes).unwrap(), mime);
            for end in 0..bytes.len() {
                assert!(
                    validate(&bytes[..end]).is_err(),
                    "accepted truncated {mime} at {end}"
                );
            }
            // Correct magic and end markers cannot disguise an invalid body.
            let mut corrupt = bytes[..8].to_vec();
            corrupt.extend_from_slice(b"garbage");
            corrupt.extend_from_slice(&bytes[bytes.len() - 12..]);
            assert!(validate(&corrupt).is_err());
        }
    }

    #[test]
    fn rejects_http_errors_including_empty_and_image_bodies() {
        let jpeg = capture(ImageFormat::Jpeg);
        for status in [404, 401, 502, 504, 304] {
            for body in [b"".as_slice(), b"screenshot failed", jpeg.as_slice()] {
                assert!(
                    validate_response(reqwest::StatusCode::from_u16(status).unwrap(), body)
                        .is_err()
                );
            }
        }
    }
}
