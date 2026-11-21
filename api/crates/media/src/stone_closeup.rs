//! A close-up of the stone in a red circle at the top right of its picture
//! (Admin › Stones › Add or Edit › Close-up), like a magnifying glass over
//! the slab.
//!
//! Drawn on the finished picture, after the slab is placed on the company's
//! background, so the circle sits over the whole picture. The close-up is cut
//! to a square from its middle to fill the circle.

use crate::images::{self, encode_jpeg};
use image::{DynamicImage, Rgb, RgbImage, imageops::FilterType};

/// Longest side of the finished picture.
const MAX_SIDE: u32 = 1920;
const JPEG_QUALITY: u8 = 88;
/// Circle across, as a share of the picture's height...
const DIAMETER_OF_HEIGHT: f64 = 0.46;
/// ...and at most this share of its width (tall pictures).
const DIAMETER_OF_WIDTH: f64 = 0.36;
/// Space to the top and right edges, as a share of the shorter side.
const MARGIN: f64 = 0.032;
/// Red ring, as a share of the circle across.
const RING: f64 = 0.012;
const RING_COLOR: [f64; 3] = [228.0, 30.0, 37.0];
/// Samples per pixel side, for a smooth round edge.
const SAMPLES: u32 = 3;

/// `picture` with `closeup` in a red circle at its top right, as a JPEG.
/// `None` when either file is not a picture this can read. CPU work: call it
/// from `tokio::task::spawn_blocking`.
pub fn add_closeup(picture: &[u8], closeup: &[u8]) -> Option<Vec<u8>> {
    let picture = images::fit_inside(&images::decode(picture).ok()?, MAX_SIDE).to_rgb8();
    let closeup = images::decode(closeup).ok()?;
    let drawn = draw(picture, &closeup);
    Some(encode_jpeg(&DynamicImage::ImageRgb8(drawn), JPEG_QUALITY))
}

/// Where the circle goes on a picture `width` by `height`: its middle and
/// radius, in pixels.
fn circle(width: f64, height: f64) -> (f64, f64, f64) {
    let radius = (height * DIAMETER_OF_HEIGHT).min(width * DIAMETER_OF_WIDTH) / 2.0;
    let margin = width.min(height) * MARGIN;
    (width - margin - radius, margin + radius, radius)
}

fn draw(mut picture: RgbImage, closeup: &DynamicImage) -> RgbImage {
    let (width, height) = (f64::from(picture.width()), f64::from(picture.height()));
    let (center_x, center_y, radius) = circle(width, height);
    if radius < 4.0 {
        return picture;
    }
    let ring = (radius * 2.0 * RING).max(2.0);
    let inner = radius - ring;
    let side = to_pixel((inner * 2.0).ceil(), u32::MAX).max(1);
    let fill = closeup
        .resize_to_fill(side, side, FilterType::Lanczos3)
        .to_rgb8();
    let left = center_x - inner;
    let top = center_y - inner;
    let step = 1.0 / f64::from(SAMPLES);
    let samples = f64::from(SAMPLES * SAMPLES);
    let x_range = to_pixel(center_x - radius, picture.width())
        ..to_pixel((center_x + radius).ceil() + 1.0, picture.width());
    let y_range = to_pixel(center_y - radius, picture.height())
        ..to_pixel((center_y + radius).ceil() + 1.0, picture.height());
    for y in y_range {
        for x in x_range.clone() {
            let under = picture.get_pixel(x, y).0.map(f64::from);
            let mut sum = [0.0; 3];
            for sample_y in 0..SAMPLES {
                for sample_x in 0..SAMPLES {
                    let px = (f64::from(sample_x) + 0.5).mul_add(step, f64::from(x));
                    let py = (f64::from(sample_y) + 0.5).mul_add(step, f64::from(y));
                    let distance = (px - center_x).hypot(py - center_y);
                    let color = if distance <= inner {
                        fill.get_pixel(to_pixel(px - left, side), to_pixel(py - top, side))
                            .0
                            .map(f64::from)
                    } else if distance <= radius {
                        RING_COLOR
                    } else {
                        under
                    };
                    for channel in 0..3 {
                        sum[channel] += color[channel];
                    }
                }
            }
            picture.put_pixel(x, y, Rgb(sum.map(|total| to_byte(total / samples))));
        }
    }
    picture
}

/// A pixel position below `max` (0 when `max` is 0).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn to_pixel(value: f64, max: u32) -> u32 {
    (value.max(0.0) as u32).min(max.saturating_sub(1))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const fn to_byte(value: f64) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    const GREY: Rgb<u8> = Rgb([100, 100, 100]);
    const BLUE: Rgb<u8> = Rgb([20, 40, 220]);

    fn drawn(width: u32, height: u32) -> RgbImage {
        let picture = RgbImage::from_pixel(width, height, GREY);
        let closeup = DynamicImage::ImageRgb8(RgbImage::from_pixel(50, 30, BLUE));
        draw(picture, &closeup)
    }

    #[test]
    fn the_closeup_fills_a_red_circle_at_the_top_right() {
        // 1000 x 600: 276 px across, 19 px from the top and right edges.
        let picture = drawn(1000, 600);
        let (x, y, radius) = circle(1000.0, 600.0);
        assert!((radius - 138.0).abs() < 0.01);
        assert!((x - 842.8).abs() < 0.01 && (y - 157.2).abs() < 0.01);
        assert_eq!(*picture.get_pixel(843, 157), BLUE);
        assert_eq!(*picture.get_pixel(843, 157 - 120), BLUE);
        // On the ring, above the middle.
        assert_eq!(*picture.get_pixel(843, 157 - 136), Rgb([228, 30, 37]));
        // Outside the circle nothing changed.
        for (x, y) in [
            (0, 0),
            (999, 0),
            (999, 599),
            (0, 599),
            (500, 300),
            (705, 20),
        ] {
            assert_eq!(*picture.get_pixel(x, y), GREY, "at {x}, {y}");
        }
    }

    #[test]
    fn a_tall_picture_gets_a_circle_that_fits_its_width() {
        let (x, _, radius) = circle(600.0, 1000.0);
        assert!((radius - 108.0).abs() < 0.01);
        assert!(x - radius > 0.0);
        let picture = drawn(600, 1000);
        assert_eq!(*picture.get_pixel(0, 0), GREY);
    }

    #[test]
    fn the_same_files_always_make_the_same_picture() {
        assert_eq!(drawn(400, 300).as_raw(), drawn(400, 300).as_raw());
    }

    #[test]
    fn unreadable_files_make_no_picture() {
        assert!(add_closeup(b"not a picture", b"not a picture").is_none());
    }
}
