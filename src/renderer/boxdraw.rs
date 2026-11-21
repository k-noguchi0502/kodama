//! Procedurally drawn box-drawing (U+2500..U+257F) and block element
//! (U+2580..U+259F) glyphs, sized exactly to the cell so lines join seamlessly.

/// Arm weights for U+2500..=U+257F as "LRUD": 0 none, 1 light, 2 heavy, 3 double.
/// `None` entries (diagonals) are left to the font.
const BOX: [Option<&str>; 128] = {
    let table: [&str; 128] = [
        "1100", "2200", "0011", "0022", "1100", "2200", "0011", "0022", // 2500
        "1100", "2200", "0011", "0022", "0101", "0201", "0102", "0202", // 2508
        "1001", "2001", "1002", "2002", "0110", "0210", "0120", "0220", // 2510
        "1010", "2010", "1020", "2020", "0111", "0211", "0121", "0112", // 2518
        "0122", "0221", "0212", "0222", "1011", "2011", "1021", "1012", // 2520
        "1022", "2021", "2012", "2022", "1101", "2101", "1201", "2201", // 2528
        "1102", "2102", "1202", "2202", "1110", "2110", "1210", "2210", // 2530
        "1120", "2120", "1220", "2220", "1111", "2111", "1211", "2211", // 2538
        "1121", "1112", "1122", "2121", "1221", "2112", "1212", "2221", // 2540
        "2212", "2122", "1222", "2222", "1100", "2200", "0011", "0022", // 2548
        "3300", "0033", "0301", "0103", "0303", "3001", "1003", "3003", // 2550
        "0310", "0130", "0330", "3010", "1030", "3030", "0311", "0133", // 2558
        "0333", "3011", "1033", "3033", "3301", "1103", "3303", "3310", // 2560
        "1130", "3330", "3311", "1133", "3333", "0101", "1001", "1010", // 2568
        "0110", "", "", "", "1000", "0010", "0100", "0001", // 2570
        "2000", "0020", "0200", "0002", "1200", "0012", "2100", "0021", // 2578
    ];
    let mut out = [None; 128];
    let mut i = 0;
    while i < 128 {
        if !table[i].is_empty() {
            out[i] = Some(table[i]);
        }
        i += 1;
    }
    out
};

struct Canvas {
    w: usize,
    h: usize,
    data: Vec<u8>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            data: vec![0; w * h],
        }
    }

    fn fill(&mut self, x0: isize, y0: isize, x1: isize, y1: isize, alpha: u8) {
        let x0 = x0.clamp(0, self.w as isize) as usize;
        let x1 = x1.clamp(0, self.w as isize) as usize;
        let y0 = y0.clamp(0, self.h as isize) as usize;
        let y1 = y1.clamp(0, self.h as isize) as usize;
        for y in y0..y1 {
            self.data[y * self.w + x0..y * self.w + x1].fill(alpha);
        }
    }

    /// Fill a rectangle given in fractions of the cell.
    fn fill_frac(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, alpha: u8) {
        let (w, h) = (self.w as f32, self.h as f32);
        self.fill(
            (x0 * w).round() as isize,
            (y0 * h).round() as isize,
            (x1 * w).round() as isize,
            (y1 * h).round() as isize,
            alpha,
        );
    }
}

impl Canvas {
    /// Anti-aliased fill of the region where `inside(x, y)` holds, with
    /// coordinates in pixels; each pixel is sampled on a 4x4 grid.
    fn fill_shape(&mut self, inside: impl Fn(f32, f32) -> bool) {
        const N: usize = 4;
        for py in 0..self.h {
            for px in 0..self.w {
                let mut hits = 0;
                for sy in 0..N {
                    for sx in 0..N {
                        let x = px as f32 + (sx as f32 + 0.5) / N as f32;
                        let y = py as f32 + (sy as f32 + 0.5) / N as f32;
                        hits += inside(x, y) as usize;
                    }
                }
                let alpha = (hits * 255 / (N * N)) as u8;
                let slot = &mut self.data[py * self.w + px];
                *slot = (*slot).max(alpha);
            }
        }
    }
}

/// Distance from `p` to the segment `a`-`b`.
fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    let (ex, ey) = (a.0 + t * dx - p.0, a.1 + t * dy - p.1);
    (ex * ex + ey * ey).sqrt()
}

pub fn is_builtin(c: char) -> bool {
    match c as u32 {
        0x2571..=0x2573 => true,
        0x2500..=0x257f => BOX[(c as u32 - 0x2500) as usize].is_some(),
        0x2580..=0x259f => true,
        0xe0b0..=0xe0bf => true,
        _ => false,
    }
}

/// Rasterize `c` into a `width` x `height` alpha bitmap.
pub fn rasterize(c: char, width: usize, height: usize, stroke: usize) -> Option<Vec<u8>> {
    let mut canvas = Canvas::new(width, height);
    match c as u32 {
        0x2504..=0x250b | 0x254c..=0x254f => draw_dashed(&mut canvas, c, stroke),
        0x256d..=0x2570 => draw_arc(&mut canvas, c, stroke),
        0x2571..=0x2573 => draw_diagonal(&mut canvas, c, stroke),
        0x2500..=0x257f => draw_box(&mut canvas, BOX[(c as u32 - 0x2500) as usize]?, stroke),
        0x2580..=0x259f => draw_block(&mut canvas, c),
        0xe0b0..=0xe0bf => draw_powerline(&mut canvas, c, stroke),
        _ => return None,
    }
    Some(canvas.data)
}

/// Powerline separators (U+E0B0..U+E0BF), drawn to the full cell height so
/// they meet the neighbouring segment backgrounds without seams.
fn draw_powerline(canvas: &mut Canvas, c: char, stroke: usize) {
    let (w, h) = (canvas.w as f32, canvas.h as f32);
    let half = stroke.max(1) as f32 / 2.0 + 0.25;
    let n = c as u32;
    // Odd code points are the outline variants of the even ones; the left-
    // pointing shapes (E0B2/3, E0B6/7) and E0BA-E0BF are mirror images.
    let mirror = matches!(
        n,
        0xe0b2 | 0xe0b3 | 0xe0b6 | 0xe0b7 | 0xe0ba | 0xe0bb | 0xe0be | 0xe0bf
    );
    let fx = move |x: f32| if mirror { w - x } else { x };
    match n {
        0xe0b0 | 0xe0b2 => canvas.fill_shape(|x, y| fx(x) <= w * (1.0 - (2.0 * y / h - 1.0).abs())),
        0xe0b1 | 0xe0b3 => canvas.fill_shape(|x, y| {
            let p = (fx(x), y);
            segment_distance(p, (0.0, 0.0), (w, h / 2.0)).min(segment_distance(
                p,
                (0.0, h),
                (w, h / 2.0),
            )) <= half
        }),
        0xe0b4 | 0xe0b6 => canvas.fill_shape(|x, y| {
            let (u, v) = (fx(x) / w, (y - h / 2.0) / (h / 2.0));
            u * u + v * v <= 1.0
        }),
        0xe0b5 | 0xe0b7 => canvas.fill_shape(|x, y| {
            let ellipse = |rx: f32, ry: f32| {
                let (u, v) = (fx(x) / rx, (y - h / 2.0) / ry);
                u * u + v * v
            };
            ellipse(w, h / 2.0) <= 1.0 && ellipse(w - 2.0 * half, h / 2.0 - 2.0 * half) >= 1.0
        }),
        // Lower triangles (E0B8 left, E0BA right) and upper ones (E0BC, E0BE).
        0xe0b8 | 0xe0ba => canvas.fill_shape(|x, y| y / h >= fx(x) / w),
        0xe0bc | 0xe0be => canvas.fill_shape(|x, y| y / h <= 1.0 - fx(x) / w),
        // Diagonal lines: backslash (E0B9, E0BF) and slash (E0BB, E0BD).
        0xe0b9 | 0xe0bf => {
            canvas.fill_shape(|x, y| segment_distance((x, y), (0.0, 0.0), (w, h)) <= half)
        }
        _ => canvas.fill_shape(|x, y| segment_distance((x, y), (0.0, h), (w, 0.0)) <= half),
    }
}

fn draw_box(canvas: &mut Canvas, arms: &str, stroke: usize) {
    let weights: Vec<u8> = arms.bytes().map(|b| b - b'0').collect();
    let (left, right, up, down) = (weights[0], weights[1], weights[2], weights[3]);
    let t = stroke as isize;
    let thickness = |w: u8| match w {
        1 => t,
        2 => t * 2,
        3 => t * 3,
        _ => 0,
    };
    let (w, h) = (canvas.w as isize, canvas.h as isize);
    let (cx, cy) = (w / 2, h / 2);

    // Arms run up to the far edge of the perpendicular stroke, so joins are
    // filled without poking out past the line they meet.
    let (vx0, vx1) = stroke_span(cx, thickness(up).max(thickness(down)));
    let (hy0, hy1) = stroke_span(cy, thickness(left).max(thickness(right)));

    // A double line is two strokes with a stroke-wide gap: lines 0 and 1 are
    // the top/bottom (horizontal) or left/right (vertical) strokes.
    let double = |centre: isize| {
        let start = centre - (3 * t) / 2;
        [(start, start + t), (start + 2 * t, start + 3 * t)]
    };
    let single = |weight: u8, centre: isize| stroke_span(centre, thickness(weight));
    let (ys, xs) = (double(cy), double(cx));
    let h_double = left == 3 || right == 3;
    let v_double = up == 3 || down == 3;

    // Double lines meeting at a join: the stroke on the side of a
    // perpendicular arm stops at the inner line, the other at the outer line.
    let (has_l, has_r, has_u, has_d) = (left != 0, right != 0, up != 0, down != 0);
    let pick = |inner: bool, inner_v: isize, outer_v: isize| if inner { inner_v } else { outer_v };

    if right != 0 {
        if right == 3 {
            for (i, (y0, y1)) in ys.into_iter().enumerate() {
                let inner = if i == 0 { has_u } else { has_d };
                let x0 = if v_double {
                    pick(inner, xs[1].0, xs[0].0)
                } else {
                    vx0
                };
                canvas.fill(x0, y0, w, y1, 255);
            }
        } else {
            let x0 = if v_double {
                pick(has_u && has_d, xs[1].0, xs[0].0)
            } else {
                vx0
            };
            let (y0, y1) = single(right, cy);
            canvas.fill(x0, y0, w, y1, 255);
        }
    }
    if left != 0 {
        if left == 3 {
            for (i, (y0, y1)) in ys.into_iter().enumerate() {
                let inner = if i == 0 { has_u } else { has_d };
                let x1 = if v_double {
                    pick(inner, xs[0].1, xs[1].1)
                } else {
                    vx1
                };
                canvas.fill(0, y0, x1, y1, 255);
            }
        } else {
            let x1 = if v_double {
                pick(has_u && has_d, xs[0].1, xs[1].1)
            } else {
                vx1
            };
            let (y0, y1) = single(left, cy);
            canvas.fill(0, y0, x1, y1, 255);
        }
    }
    if down != 0 {
        if down == 3 {
            for (i, (x0, x1)) in xs.into_iter().enumerate() {
                let inner = if i == 0 { has_l } else { has_r };
                let y0 = if h_double {
                    pick(inner, ys[1].0, ys[0].0)
                } else {
                    hy0
                };
                canvas.fill(x0, y0, x1, h, 255);
            }
        } else {
            let y0 = if h_double {
                pick(has_l && has_r, ys[1].0, ys[0].0)
            } else {
                hy0
            };
            let (x0, x1) = single(down, cx);
            canvas.fill(x0, y0, x1, h, 255);
        }
    }
    if up != 0 {
        if up == 3 {
            for (i, (x0, x1)) in xs.into_iter().enumerate() {
                let inner = if i == 0 { has_l } else { has_r };
                let y1 = if h_double {
                    pick(inner, ys[0].1, ys[1].1)
                } else {
                    hy1
                };
                canvas.fill(x0, 0, x1, y1, 255);
            }
        } else {
            let y1 = if h_double {
                pick(has_l && has_r, ys[0].1, ys[1].1)
            } else {
                hy1
            };
            let (x0, x1) = single(up, cx);
            canvas.fill(x0, 0, x1, y1, 255);
        }
    }
}

/// Pixel range of a straight stroke of `thickness` centred on `centre`;
/// shared by every drawing routine so all lines through a cell coincide.
fn stroke_span(centre: isize, thickness: isize) -> (isize, isize) {
    (centre - thickness / 2, centre - thickness / 2 + thickness)
}

/// Dashed lines: 2, 3 or 4 dashes per cell, each centred in its slot so the
/// pattern continues evenly into the neighbouring cell.
fn draw_dashed(canvas: &mut Canvas, c: char, stroke: usize) {
    let n = c as u32;
    let (dashes, heavy, vertical) = match n {
        0x2504..=0x2507 => (3, n & 1 == 1, n >= 0x2506),
        0x2508..=0x250b => (4, n & 1 == 1, n >= 0x250a),
        _ => (2, n & 1 == 1, n >= 0x254e),
    };
    let t = stroke as isize * if heavy { 2 } else { 1 };
    let (w, h) = (canvas.w as isize, canvas.h as isize);
    let len = if vertical { h } else { w } as f32;
    let slot = len / dashes as f32;
    let gap = (slot * 0.4).round().max(1.0);
    let (a0, a1) = stroke_span(if vertical { w / 2 } else { h / 2 }, t);
    for i in 0..dashes {
        let start = (i as f32 * slot + gap / 2.0).round() as isize;
        let end = ((i + 1) as f32 * slot - gap / 2.0).round() as isize;
        if vertical {
            canvas.fill(a0, start, a1, end, 255);
        } else {
            canvas.fill(start, a0, end, a1, 255);
        }
    }
}

/// Rounded corners: a quarter circle joining the same centre lines that the
/// straight box-drawing strokes use, continued straight to the cell edges.
fn draw_arc(canvas: &mut Canvas, c: char, stroke: usize) {
    let t = stroke.max(1) as isize;
    let (w, h) = (canvas.w as isize, canvas.h as isize);
    let (x0, x1) = stroke_span(w / 2, t);
    let (y0, y1) = stroke_span(h / 2, t);
    let (fx, fy) = ((x0 + x1) as f32 / 2.0, (y0 + y1) as f32 / 2.0);
    let (w, h) = (w as f32, h as f32);
    // Direction of the horizontal arm (+1 right) and vertical arm (+1 down).
    let (sx, sy): (f32, f32) = match c as u32 {
        0x256d => (1.0, 1.0),
        0x256e => (-1.0, 1.0),
        0x256f => (-1.0, -1.0),
        _ => (1.0, -1.0),
    };
    let reach_x = if sx > 0.0 { w - fx } else { fx };
    let reach_y = if sy > 0.0 { h - fy } else { fy };
    let r = reach_x.min(reach_y);
    let (cx, cy) = (fx + sx * r, fy + sy * r);
    let half = t as f32 / 2.0;
    canvas.fill_shape(|x, y| {
        let (dx, dy) = (x - cx, y - cy);
        if dx * sx <= 0.0 && dy * sy <= 0.0 {
            ((dx * dx + dy * dy).sqrt() - r).abs() <= half
        } else if dx * sx > 0.0 && dy * sy <= 0.0 {
            // Past the arc horizontally: the straight arm to the edge.
            (y - fy).abs() <= half
        } else if dy * sy > 0.0 && dx * sx <= 0.0 {
            (x - fx).abs() <= half
        } else {
            false
        }
    });
}

/// Diagonals run exactly corner to corner so they meet the next cell's line.
fn draw_diagonal(canvas: &mut Canvas, c: char, stroke: usize) {
    let (w, h) = (canvas.w as f32, canvas.h as f32);
    let half = stroke.max(1) as f32 / 2.0 + 0.2;
    let n = c as u32;
    canvas.fill_shape(|x, y| {
        let rising = segment_distance((x, y), (0.0, h), (w, 0.0)) <= half;
        let falling = segment_distance((x, y), (0.0, 0.0), (w, h)) <= half;
        match n {
            0x2571 => rising,
            0x2572 => falling,
            _ => rising || falling,
        }
    });
}

fn draw_block(canvas: &mut Canvas, c: char) {
    let n = c as u32;
    match n {
        0x2580 => canvas.fill_frac(0.0, 0.0, 1.0, 0.5, 255),
        0x2581..=0x2588 => {
            let eighths = (n - 0x2580) as f32 / 8.0;
            canvas.fill_frac(0.0, 1.0 - eighths, 1.0, 1.0, 255);
        }
        0x2589..=0x258f => {
            let eighths = (0x2590 - n) as f32 / 8.0;
            canvas.fill_frac(0.0, 0.0, eighths, 1.0, 255);
        }
        0x2590 => canvas.fill_frac(0.5, 0.0, 1.0, 1.0, 255),
        0x2591 => canvas.fill_frac(0.0, 0.0, 1.0, 1.0, 64),
        0x2592 => canvas.fill_frac(0.0, 0.0, 1.0, 1.0, 128),
        0x2593 => canvas.fill_frac(0.0, 0.0, 1.0, 1.0, 192),
        0x2594 => canvas.fill_frac(0.0, 0.0, 1.0, 0.125, 255),
        0x2595 => canvas.fill_frac(0.875, 0.0, 1.0, 1.0, 255),
        _ => {
            // Quadrants: bits = upper-left, upper-right, lower-left, lower-right.
            let bits: u8 = match n {
                0x2596 => 0b0010,
                0x2597 => 0b0001,
                0x2598 => 0b1000,
                0x2599 => 0b1011,
                0x259a => 0b1001,
                0x259b => 0b1110,
                0x259c => 0b1101,
                0x259d => 0b0100,
                0x259e => 0b0110,
                0x259f => 0b0111,
                _ => 0,
            };
            if bits & 0b1000 != 0 {
                canvas.fill_frac(0.0, 0.0, 0.5, 0.5, 255);
            }
            if bits & 0b0100 != 0 {
                canvas.fill_frac(0.5, 0.0, 1.0, 0.5, 255);
            }
            if bits & 0b0010 != 0 {
                canvas.fill_frac(0.0, 0.5, 0.5, 1.0, 255);
            }
            if bits & 0b0001 != 0 {
                canvas.fill_frac(0.5, 0.5, 1.0, 1.0, 255);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_line_spans_cell() {
        let bitmap = rasterize('─', 8, 16, 1).unwrap();
        let row = &bitmap[8 * 8..9 * 8];
        assert!(row.iter().all(|&a| a == 255));
        assert!(bitmap[..8].iter().all(|&a| a == 0));
    }

    #[test]
    fn corner_joins_at_centre() {
        let bitmap = rasterize('┌', 8, 16, 1).unwrap();
        // Right arm on row 8 from the centre, down arm in column 4.
        assert_eq!(bitmap[8 * 8 + 7], 255);
        assert_eq!(bitmap[15 * 8 + 4], 255);
        assert_eq!(bitmap[8 * 8], 0);
        assert_eq!(bitmap[4], 0);
    }

    #[test]
    fn corners_do_not_overshoot_the_lines_they_join() {
        for (w, h) in [(8, 16), (9, 19), (10, 20)] {
            for stroke in 1..=3 {
                for (light, heavy) in [("─", "│"), ("━", "┃")] {
                    let line = rasterize(light.chars().next().unwrap(), w, h, stroke).unwrap();
                    let bar = rasterize(heavy.chars().next().unwrap(), w, h, stroke).unwrap();
                    let rows: Vec<usize> = (0..h).filter(|&y| line[y * w] == 255).collect();
                    let cols: Vec<usize> = (0..w).filter(|&x| bar[x] == 255).collect();
                    let corners = if light == "─" {
                        "┌┐└┘"
                    } else {
                        "┏┓┗┛"
                    };
                    for c in corners.chars() {
                        let b = rasterize(c, w, h, stroke).unwrap();
                        let goes_left = matches!(c, '┐' | '┘' | '┓' | '┛');
                        let goes_up = matches!(c, '└' | '┘' | '┗' | '┛');
                        let (x0, x1) = (cols[0], *cols.last().unwrap());
                        let (y0, y1) = (rows[0], *rows.last().unwrap());
                        // Lit pixels stay inside the two line bands and never
                        // run past the line they turn into.
                        for y in 0..h {
                            for x in 0..w {
                                if b[y * w + x] == 0 {
                                    continue;
                                }
                                let on_row =
                                    rows.contains(&y) && if goes_left { x <= x1 } else { x >= x0 };
                                let on_col =
                                    cols.contains(&x) && if goes_up { y <= y1 } else { y >= y0 };
                                assert!(
                                    on_row || on_col,
                                    "{c} {w}x{h} stroke {stroke}: stray pixel at ({x},{y})"
                                );
                            }
                        }
                        for &y in &rows {
                            for &x in &cols {
                                assert_eq!(b[y * w + x], 255, "{c}: join is filled");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn double_corner_has_open_inner_gap() {
        // ╔ in a 12x12 cell with stroke 1: double strokes at 5 and 7, gap at 6.
        let b = rasterize('╔', 12, 12, 1).unwrap();
        let at = |x: usize, y: usize| b[y * 12 + x];
        assert_eq!(at(11, 5), 255);
        assert_eq!(at(11, 7), 255);
        assert_eq!(at(11, 6), 0);
        assert_eq!(at(5, 11), 255);
        assert_eq!(at(7, 11), 255);
        assert_eq!(at(6, 11), 0);
        // The outer corner closes, the inner corner stays open.
        assert_eq!(at(5, 5), 255);
        assert_eq!(at(6, 6), 0);
        // Nothing to the left of / above the corner.
        assert_eq!(at(0, 5), 0);
        assert_eq!(at(5, 0), 0);
    }

    #[test]
    fn rounded_corner_meets_straight_lines() {
        let (w, h) = (10, 22);
        let corner = rasterize('╭', w, h, 1).unwrap();
        let line = rasterize('─', w, h, 1).unwrap();
        let bar = rasterize('│', w, h, 1).unwrap();
        let at = |b: &[u8], x: usize, y: usize| b[y * w + x];
        let row = (0..h).find(|&y| at(&line, 0, y) == 255).unwrap();
        let col = (0..w).find(|&x| at(&bar, x, 0) == 255).unwrap();
        // The arc leaves the cell on the same row/column as ─ and │.
        assert!(at(&corner, w - 1, row) > 128);
        assert_eq!(at(&corner, col, h - 1), 255);
        // Nothing on the open sides, and the corner itself is cut.
        assert_eq!(at(&corner, 0, row), 0);
        assert_eq!(at(&corner, col, 0), 0);
        assert_eq!(at(&corner, col, row), 0);
    }

    #[test]
    fn dashes_have_gaps_and_share_the_line_row() {
        let (w, h) = (12, 22);
        let dashed = rasterize('┄', w, h, 1).unwrap();
        let line = rasterize('─', w, h, 1).unwrap();
        let row = (0..h).find(|&y| line[y * w] == 255).unwrap();
        let lit: Vec<bool> = (0..w).map(|x| dashed[row * w + x] == 255).collect();
        let dashes = lit.windows(2).filter(|p| !p[0] && p[1]).count() + lit[0] as usize;
        assert_eq!(dashes, 3);
        assert!(
            !lit[0] && !lit[w - 1],
            "gaps split evenly across cell edges"
        );
        let vertical = rasterize('┆', w, h, 1).unwrap();
        assert!(vertical.contains(&255));
    }

    #[test]
    fn diagonals_reach_the_corners() {
        let (w, h) = (10, 22);
        let b = rasterize('╱', w, h, 1).unwrap();
        assert!(b[w - 1] > 0, "top-right corner");
        assert!(b[(h - 1) * w] > 0, "bottom-left corner");
        assert_eq!(b[0], 0);
    }

    #[test]
    fn powerline_arrow_fills_cell_edge() {
        let (w, h) = (10, 22);
        let b = rasterize('\u{e0b0}', w, h, 1).unwrap();
        let at = |x: usize, y: usize| b[y * w + x];
        // The flat side spans the cell height (the corner pixels are partly
        // covered by the anti-aliased apex edges).
        assert!((1..h - 1).all(|y| at(0, y) == 255));
        assert!(at(0, 0) > 0 && at(0, h - 1) > 0);
        assert!(at(w - 1, h / 2) > 128);
        assert_eq!(at(w - 1, 0), 0);
        let left = rasterize('\u{e0b2}', w, h, 1).unwrap();
        assert!((1..h - 1).all(|y| left[y * w + w - 1] == 255));
        assert_eq!(left[0], 0);
    }

    #[test]
    fn full_block() {
        assert!(rasterize('█', 4, 4, 1).unwrap().iter().all(|&a| a == 255));
    }
}
