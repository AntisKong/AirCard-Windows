//! Small resolution-independent Air logotype rasterized for Windows icons.

fn segment_distance(x: f32, y: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let dx = bx - ax;
    let dy = by - ay;
    let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((x - ax - t * dx).powi(2) + (y - ay - t * dy).powi(2)).sqrt()
}

fn mark(x: f32, y: f32) -> bool {
    let strokes = [
        (10.0, 47.0, 21.0, 17.0), // A
        (21.0, 17.0, 32.0, 47.0),
        (15.0, 36.0, 27.0, 36.0),
        (37.0, 31.0, 37.0, 47.0), // i
        (46.0, 31.0, 46.0, 47.0), // r
        (46.0, 37.0, 52.0, 31.0),
    ];
    strokes.iter().any(|&(ax, ay, bx, by)| segment_distance(x, y, ax, ay, bx, by) <= 2.0)
        || ((x - 37.0).powi(2) + (y - 23.0).powi(2)).sqrt() <= 2.3
}

pub fn rgba(size: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut coverage = 0;
            let mut inside = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = (px as f32 + (sx as f32 + 0.5) / 4.0) * 64.0 / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / 4.0) * 64.0 / size as f32;
                    let cx = x.clamp(11.0, 53.0);
                    let cy = y.clamp(11.0, 53.0);
                    if (x - cx).powi(2) + (y - cy).powi(2) <= 11.0_f32.powi(2) {
                        inside += 1;
                        if mark(x, y) { coverage += 1; }
                    }
                }
            }
            let alpha = (inside * 255 / 16) as u8;
            let white = coverage as f32 / inside.max(1) as f32;
            data.extend_from_slice(&[
                (28.0 + white * 203.0) as u8,
                (32.0 + white * 133.0) as u8,
                (32.0 + white * 25.0) as u8,
                alpha,
            ]);
        }
    }
    data
}

#[cfg(test)]
mod tests {
    #[test]
    fn icon_has_expected_pixels() {
        let pixels = super::rgba(64);
        assert_eq!(pixels.len(), 64 * 64 * 4);
        assert_eq!(pixels[3], 0);
        assert_eq!(pixels[(32 * 64 + 32) * 4 + 3], 255);
    }
}
