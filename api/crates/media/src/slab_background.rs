//! A stone picture on the company's own background (Admin › Stones ›
//! Settings › Slab picture background).
//!
//! Nothing is generated: the background stays exactly as the company
//! uploaded it, and the stone picture is stretched into the four-corner spot
//! the company marked on it, with a soft shadow around it. The same
//! background and spot always give the same picture.

use crate::images::{self, encode_jpeg};
use image::{DynamicImage, Rgb, RgbImage, imageops::FilterType};

/// The four corners of the spot as fractions of the background (`[x, y]`,
/// 0 to 1), clockwise from top left.
pub type Spot = [[f64; 2]; 4];

/// Longest side of the finished picture.
const MAX_SIDE: u32 = 1920;
const JPEG_QUALITY: u8 = 88;
/// Smallest spot, as a share of the background's area.
const MIN_SPOT_AREA: f64 = 0.01;
/// Samples per pixel side, for smooth slab edges.
const SAMPLES: u32 = 3;
/// How far the shadow reaches, as a share of the background's width.
const SHADOW_REACH: f64 = 0.012;
/// How dark the shadow is right at the slab's edge.
const SHADOW_STRENGTH: f64 = 0.28;

/// Whether the corners are inside the picture and make a real four-sided
/// spot: clockwise from top left, no crossed sides, not tiny.
pub fn valid_spot(spot: &Spot) -> bool {
    let inside = spot
        .iter()
        .flatten()
        .all(|value| value.is_finite() && (0.0..=1.0).contains(value));
    if !inside {
        return false;
    }
    let turns_right = (0..4).all(|at| {
        let [a, b, c] = [spot[at], spot[(at + 1) % 4], spot[(at + 2) % 4]];
        (b[0] - a[0]).mul_add(c[1] - b[1], -((b[1] - a[1]) * (c[0] - b[0]))) > 1e-6
    });
    let area = (0..4)
        .map(|at| {
            let [a, b] = [spot[at], spot[(at + 1) % 4]];
            a[0].mul_add(b[1], -(b[0] * a[1]))
        })
        .sum::<f64>()
        / 2.0;
    turns_right && area >= MIN_SPOT_AREA
}

/// The stone picture on the background, as a JPEG. `None` when the spot is
/// not valid or either file is not a picture this can read. CPU work: call
/// it from `tokio::task::spawn_blocking`.
pub fn frame(background: &[u8], slab: &[u8], spot: &Spot) -> Option<Vec<u8>> {
    if !valid_spot(spot) {
        return None;
    }
    let background = images::fit_inside(&images::decode(background).ok()?, MAX_SIDE).to_rgb8();
    let slab = images::decode(slab).ok()?;
    let framed = place(background, &slab, spot);
    Some(encode_jpeg(&DynamicImage::ImageRgb8(framed), JPEG_QUALITY))
}

/// Draws `slab` into `spot` of `background`.
fn place(mut background: RgbImage, slab: &DynamicImage, spot: &Spot) -> RgbImage {
    let (width, height) = (
        f64::from(background.width()),
        f64::from(background.height()),
    );
    let corners = spot.map(|[x, y]| [x * width, y * height]);
    let slab = sized_for(slab, &corners);
    shade_around(&mut background, &corners, width * SHADOW_REACH);
    let to_slab = SquareFromQuad::new(&corners);
    let (left, top, right, bottom) = bounds(&corners, 0.0, &background);
    let step = 1.0 / f64::from(SAMPLES);
    for y in top..bottom {
        for x in left..right {
            let mut sum = [0.0_f64; 3];
            let mut hits = 0_u32;
            for sample_y in 0..SAMPLES {
                for sample_x in 0..SAMPLES {
                    let at_x = (f64::from(sample_x) + 0.5).mul_add(step, f64::from(x));
                    let at_y = (f64::from(sample_y) + 0.5).mul_add(step, f64::from(y));
                    let Some((u, v)) = to_slab.map(at_x, at_y) else {
                        continue;
                    };
                    let color = sample(&slab, u, v);
                    for (total, part) in sum.iter_mut().zip(color) {
                        *total += part;
                    }
                    hits += 1;
                }
            }
            if hits == 0 {
                continue;
            }
            let cover = f64::from(hits) / f64::from(SAMPLES * SAMPLES);
            let pixel = background.get_pixel_mut(x, y);
            for (channel, total) in pixel.0.iter_mut().zip(sum) {
                let slab_part = total / f64::from(hits);
                *channel = to_byte(slab_part.mul_add(cover, f64::from(*channel) * (1.0 - cover)));
            }
        }
    }
    background
}

/// The slab picture at about the size of the spot, so drawing it there reads
/// each of its pixels about once (a 36 MP photo would shimmer otherwise).
fn sized_for(slab: &DynamicImage, corners: &[[f64; 2]; 4]) -> RgbImage {
    let side =
        |a: usize, b: usize| (corners[a][0] - corners[b][0]).hypot(corners[a][1] - corners[b][1]);
    let width = to_side(side(0, 1).max(side(3, 2)));
    let height = to_side(side(0, 3).max(side(1, 2)));
    if slab.width() <= width && slab.height() <= height {
        return slab.to_rgb8();
    }
    let (width, height) = (width.min(slab.width()), height.min(slab.height()));
    if slab.width() / 4 > width && slab.height() / 4 > height {
        // Box-averaged first, as thumbnails are: Lanczos3 over a huge photo is slow.
        return slab
            .thumbnail_exact(width * 2, height * 2)
            .resize_exact(width, height, FilterType::Lanczos3)
            .to_rgb8();
    }
    slab.resize_exact(width, height, FilterType::Lanczos3)
        .to_rgb8()
}

/// Darkens the background next to the spot, fading out over `reach` pixels.
fn shade_around(background: &mut RgbImage, corners: &[[f64; 2]; 4], reach: f64) {
    if reach < 1.0 {
        return;
    }
    let (left, top, right, bottom) = bounds(corners, reach, background);
    for y in top..bottom {
        for x in left..right {
            let distance = distance_outside(corners, f64::from(x) + 0.5, f64::from(y) + 0.5);
            if distance >= reach {
                continue;
            }
            let near = 1.0 - distance / reach;
            let keep = (SHADOW_STRENGTH * near).mul_add(-near, 1.0);
            for channel in &mut background.get_pixel_mut(x, y).0 {
                *channel = to_byte(f64::from(*channel) * keep);
            }
        }
    }
}

/// How far a point is outside the spot; 0 inside it.
fn distance_outside(corners: &[[f64; 2]; 4], x: f64, y: f64) -> f64 {
    let mut inside = true;
    let mut nearest = f64::MAX;
    for at in 0..4 {
        let [a, b] = [corners[at], corners[(at + 1) % 4]];
        let (side_x, side_y) = (b[0] - a[0], b[1] - a[1]);
        let (to_x, to_y) = (x - a[0], y - a[1]);
        if side_x.mul_add(to_y, -(side_y * to_x)) < 0.0 {
            inside = false;
        }
        let length = side_x.mul_add(side_x, side_y * side_y);
        let along = if length > 0.0 {
            (to_x.mul_add(side_x, to_y * side_y) / length).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (near_x, near_y) = (along.mul_add(side_x, a[0]), along.mul_add(side_y, a[1]));
        nearest = nearest.min((x - near_x).hypot(y - near_y));
    }
    if inside { 0.0 } else { nearest }
}

/// The pixel box around the spot, `margin` wider, cut to the picture.
fn bounds(corners: &[[f64; 2]; 4], margin: f64, image: &RgbImage) -> (u32, u32, u32, u32) {
    let low = |axis: usize| {
        corners
            .iter()
            .map(|corner| corner[axis])
            .fold(f64::MAX, f64::min)
    };
    let high = |axis: usize| {
        corners
            .iter()
            .map(|corner| corner[axis])
            .fold(f64::MIN, f64::max)
    };
    (
        to_pixel((low(0) - margin).floor(), image.width()),
        to_pixel((low(1) - margin).floor(), image.height()),
        to_pixel((high(0) + margin).ceil(), image.width()),
        to_pixel((high(1) + margin).ceil(), image.height()),
    )
}

/// The slab's color at (`u`, `v`), each 0 to 1 across the picture, blended
/// from the four nearest pixels.
fn sample(slab: &RgbImage, u: f64, v: f64) -> [f64; 3] {
    let x = u
        .mul_add(f64::from(slab.width()), -0.5)
        .clamp(0.0, f64::from(slab.width() - 1));
    let y = v
        .mul_add(f64::from(slab.height()), -0.5)
        .clamp(0.0, f64::from(slab.height() - 1));
    let (left, top) = (
        to_pixel(x.floor(), slab.width() - 1),
        to_pixel(y.floor(), slab.height() - 1),
    );
    let (right, bottom) = (
        (left + 1).min(slab.width() - 1),
        (top + 1).min(slab.height() - 1),
    );
    let (across, down) = (x - f64::from(left), y - f64::from(top));
    let color = |x: u32, y: u32| -> Rgb<u8> { *slab.get_pixel(x, y) };
    let upper_pair = (color(left, top), color(right, top));
    let lower_pair = (color(left, bottom), color(right, bottom));
    let blend = |pair: (Rgb<u8>, Rgb<u8>), channel: usize| {
        f64::from(pair.1.0[channel]).mul_add(across, f64::from(pair.0.0[channel]) * (1.0 - across))
    };
    let mut out = [0.0; 3];
    for (channel, value) in out.iter_mut().enumerate() {
        *value =
            blend(lower_pair, channel).mul_add(down, blend(upper_pair, channel) * (1.0 - down));
    }
    out
}

/// Maps a point of the background to the slab picture (0 to 1 each way):
/// the reverse of the perspective that puts the unit square on the spot.
struct SquareFromQuad {
    rows: [[f64; 3]; 3],
}

impl SquareFromQuad {
    #[allow(clippy::many_single_char_names)]
    fn new(corners: &[[f64; 2]; 4]) -> Self {
        let [[x0, y0], [x1, y1], [x2, y2], [x3, y3]] = *corners;
        let (dx1, dx2, sum_x) = (x1 - x2, x3 - x2, x0 - x1 + x2 - x3);
        let (dy1, dy2, sum_y) = (y1 - y2, y3 - y2, y0 - y1 + y2 - y3);
        // Never 0 for a spot that passed `valid_spot`.
        let bottom = dx1.mul_add(dy2, -(dx2 * dy1));
        let g = sum_x.mul_add(dy2, -(dx2 * sum_y)) / bottom;
        let h = dx1.mul_add(sum_y, -(sum_x * dy1)) / bottom;
        let (a, b, c) = (g.mul_add(x1, x1 - x0), h.mul_add(x3, x3 - x0), x0);
        let (d, e, f) = (g.mul_add(y1, y1 - y0), h.mul_add(y3, y3 - y0), y0);
        // The inverse of [[a, b, c], [d, e, f], [g, h, 1]], up to scale.
        Self {
            rows: [
                [f.mul_add(-h, e), c.mul_add(h, -b), b.mul_add(f, -(c * e))],
                [f.mul_add(g, -d), c.mul_add(-g, a), c.mul_add(d, -(a * f))],
                [
                    d.mul_add(h, -(e * g)),
                    b.mul_add(g, -(a * h)),
                    a.mul_add(e, -(b * d)),
                ],
            ],
        }
    }

    /// Where (`x`, `y`) falls on the slab; `None` outside the spot.
    fn map(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let row =
            |at: usize| self.rows[at][0].mul_add(x, self.rows[at][1].mul_add(y, self.rows[at][2]));
        let scale = row(2);
        if scale.abs() < f64::EPSILON {
            return None;
        }
        let (u, v) = (row(0) / scale, row(1) / scale);
        ((0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v)).then_some((u, v))
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const fn to_byte(value: f64) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

/// A pixel position, kept between 0 and `max`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn to_pixel(value: f64, max: u32) -> u32 {
    value.clamp(0.0, f64::from(max)) as u32
}

/// A side length in whole pixels, at least 1.
fn to_side(length: f64) -> u32 {
    to_pixel(length.ceil(), MAX_SIDE * 2).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GREY: Rgb<u8> = Rgb([100, 100, 100]);
    const RED: Rgb<u8> = Rgb([200, 30, 30]);
    const SPOT: Spot = [[0.25, 0.3], [0.75, 0.25], [0.75, 0.8], [0.25, 0.8]];

    fn placed() -> RgbImage {
        let background = RgbImage::from_pixel(400, 300, GREY);
        let slab = DynamicImage::ImageRgb8(RgbImage::from_pixel(64, 40, RED));
        place(background, &slab, &SPOT)
    }

    #[test]
    fn the_slab_fills_the_spot_and_the_rest_of_the_background_is_untouched() {
        let framed = placed();
        // Middle of the spot, and just inside each of its corners.
        for (x, y) in [(200, 160), (103, 93), (297, 79), (297, 237), (103, 237)] {
            assert_eq!(*framed.get_pixel(x, y), RED, "at {x}, {y}");
        }
        // Far from the spot nothing changed, the shadow included.
        for (x, y) in [
            (0, 0),
            (399, 299),
            (50, 150),
            (350, 150),
            (200, 20),
            (200, 280),
        ] {
            assert_eq!(*framed.get_pixel(x, y), GREY, "at {x}, {y}");
        }
    }

    #[test]
    fn a_soft_shadow_darkens_only_the_edge_of_the_spot() {
        let framed = placed();
        // 400 px wide: the shadow reaches about 5 px.
        let beside = framed.get_pixel(98, 160).0[0];
        assert!(
            beside < 100 && beside > 60,
            "shadow next to the slab: {beside}"
        );
        assert_eq!(*framed.get_pixel(90, 160), GREY);
    }

    #[test]
    fn the_same_files_always_make_the_same_picture() {
        assert_eq!(placed().as_raw(), placed().as_raw());
    }

    #[test]
    fn the_slab_keeps_its_sides_in_a_slanted_spot() {
        // Left half blue, right half yellow: left stays left, top stays top.
        let mut slab = RgbImage::from_pixel(80, 40, Rgb([20, 20, 220]));
        for y in 0..40 {
            for x in 40..80 {
                slab.put_pixel(x, y, Rgb([230, 220, 20]));
            }
        }
        let framed = place(
            RgbImage::from_pixel(400, 300, GREY),
            &DynamicImage::ImageRgb8(slab),
            &SPOT,
        );
        assert_eq!(*framed.get_pixel(130, 160), Rgb([20, 20, 220]));
        assert_eq!(*framed.get_pixel(270, 160), Rgb([230, 220, 20]));
    }

    #[test]
    fn a_spot_must_be_four_sided_clockwise_and_inside_the_picture() {
        assert!(valid_spot(&SPOT));
        // Crossed sides.
        assert!(!valid_spot(&[
            [0.25, 0.3],
            [0.75, 0.8],
            [0.75, 0.25],
            [0.25, 0.8]
        ]));
        // Counter-clockwise.
        assert!(!valid_spot(&[
            [0.25, 0.3],
            [0.25, 0.8],
            [0.75, 0.8],
            [0.75, 0.25]
        ]));
        // Outside the picture.
        assert!(!valid_spot(&[
            [-0.1, 0.3],
            [0.75, 0.25],
            [0.75, 0.8],
            [0.25, 0.8]
        ]));
        assert!(!valid_spot(&[
            [f64::NAN, 0.3],
            [0.75, 0.25],
            [0.75, 0.8],
            [0.25, 0.8]
        ]));
        // Too small to hold a slab.
        assert!(!valid_spot(&[
            [0.5, 0.5],
            [0.55, 0.5],
            [0.55, 0.55],
            [0.5, 0.55]
        ]));
    }

    #[test]
    fn unreadable_files_make_no_picture() {
        assert!(frame(b"not a picture", b"not a picture", &SPOT).is_none());
    }
}
