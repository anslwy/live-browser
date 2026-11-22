//! Shared validation at every screenshot boundary (CDP, HTTP, MCP and CLI).
use anyhow::{bail, Context, Result};
use image::ImageFormat;

pub(crate) fn validate(bytes: &[u8]) -> Result<&'static str> {
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
    reader.decode().context("corrupted screenshot data")?;
    Ok(mime)
}

/// Reject HTTP errors even if their bodies happen to contain valid images.
pub(crate) fn validate_response(status: reqwest::StatusCode, bytes: &[u8]) -> Result<&'static str> {
    if !status.is_success() {
        let detail: String = String::from_utf8_lossy(bytes).chars().take(500).collect();
        bail!("screenshot HTTP {status}: {detail}");
    }
    validate(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(format: ImageFormat) -> Vec<u8> {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            2,
            2,
            image::Rgb([1, 2, 3]),
        ));
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
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
