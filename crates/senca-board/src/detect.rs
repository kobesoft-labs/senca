//! 板の四辺形の自動検出（ベストエフォート）。
//!
//! 1. 輝度を幅 480 前後に縮め、ぼかして Sobel の勾配を求める。
//! 2. 強い勾配の画素で Hough 変換（勾配の向き ±10° だけに投票）。
//! 3. 横向き・縦向きの直線の候補から 2 本ずつ選んで四辺形を作り、辺に沿った勾配の支持率で点数を付ける。
//! 4. 元の解像度で各辺の法線方向に勾配の最大を探し、直線を当てはめ直して角を求める。
//!
//! 支持率（信頼度）が低ければ None を返す（手動で 4 隅を指定してもらう）。

use senca_image::filter::{gaussian_blur_f32, sobel};
use senca_image::scale::{ScaleFilter, scale_plane};
use senca_image::{Plane, PlaneF32, Result};

use crate::homography::Point;

/// 検出のパラメーター。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetectParams {
    /// 解析の幅。
    pub analysis_width: usize,
    /// 画面に対する板の面積の下限（割合）。
    pub min_area_fraction: f64,
    /// これ未満の信頼度なら None。
    pub min_confidence: f64,
}

impl Default for DetectParams {
    fn default() -> Self {
        Self {
            analysis_width: 480,
            min_area_fraction: 0.10,
            min_confidence: 0.55,
        }
    }
}

/// 検出結果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetectedBoard {
    /// 左上・右上・右下・左下（入力の輝度の連続座標）。
    pub corners: [Point; 4],
    /// 0〜1。四辺に沿って勾配が続いている割合の最小値。
    pub confidence: f64,
}

/// 直線 x cosθ + y sinθ = ρ。
#[derive(Debug, Clone, Copy)]
struct Line {
    theta: f64,
    rho: f64,
}

impl Line {
    fn intersect(&self, o: &Line) -> Option<Point> {
        let (a1, b1, a2, b2) = (
            self.theta.cos(),
            self.theta.sin(),
            o.theta.cos(),
            o.theta.sin(),
        );
        let det = a1 * b2 - a2 * b1;
        if det.abs() < 1e-6 {
            return None;
        }
        Some(Point::new(
            (self.rho * b2 - o.rho * b1) / det,
            (a1 * o.rho - a2 * self.rho) / det,
        ))
    }
}

struct Grad {
    w: usize,
    h: usize,
    mag: Vec<f32>,
    gx: Vec<f32>,
    gy: Vec<f32>,
}

fn gradients(p: &Plane, sigma: f32) -> Grad {
    let f = gaussian_blur_f32(&PlaneF32::from_plane(p), sigma);
    let (gx, gy) = sobel(&f);
    let mag = gx
        .data
        .iter()
        .zip(&gy.data)
        .map(|(a, b)| (a * a + b * b).sqrt())
        .collect();
    Grad {
        w: p.width(),
        h: p.height(),
        mag,
        gx: gx.data,
        gy: gy.data,
    }
}

/// 勾配の向きに沿った極大（細線化）か。
fn is_ridge(g: &Grad, x: usize, y: usize) -> bool {
    let i = y * g.w + x;
    let m = g.mag[i];
    let (ax, ay) = (g.gx[i].abs(), g.gy[i].abs());
    let (dx, dy): (isize, isize) = if ax > 2.0 * ay {
        (1, 0)
    } else if ay > 2.0 * ax {
        (0, 1)
    } else if (g.gx[i] > 0.0) == (g.gy[i] > 0.0) {
        (1, 1)
    } else {
        (1, -1)
    };
    let at = |xx: isize, yy: isize| {
        if xx < 0 || yy < 0 || xx >= g.w as isize || yy >= g.h as isize {
            0.0
        } else {
            g.mag[yy as usize * g.w + xx as usize]
        }
    };
    let (xi, yi) = (x as isize, y as isize);
    m >= at(xi + dx, yi + dy) && m > at(xi - dx, yi - dy)
}

fn hough(g: &Grad, thr: f32) -> Vec<Line> {
    let nt = 180usize;
    let diag = ((g.w * g.w + g.h * g.h) as f64).sqrt().ceil() as usize;
    let nr = 2 * diag + 1;
    let mut acc = vec![0f32; nt * nr];
    let cs: Vec<(f64, f64)> = (0..nt)
        .map(|t| {
            let th = t as f64 * std::f64::consts::PI / nt as f64;
            (th.cos(), th.sin())
        })
        .collect();
    for y in 0..g.h {
        for x in 0..g.w {
            let i = y * g.w + x;
            let m = g.mag[i];
            if m < thr || !is_ridge(g, x, y) {
                continue;
            }
            let mut phi = (g.gy[i] as f64).atan2(g.gx[i] as f64).to_degrees();
            if phi < 0.0 {
                phi += 180.0;
            }
            let c = phi.round() as isize;
            for dt in -10..=10isize {
                let t = (c + dt).rem_euclid(nt as isize) as usize;
                let (co, si) = cs[t];
                let rho = (x as f64 + 0.5) * co + (y as f64 + 0.5) * si;
                let r = (rho.round() as isize + diag as isize) as usize;
                if r < nr {
                    // 長い直線を優先するため、強さではなく画素の数で投票する。
                    acc[t * nr + r] += 1.0;
                }
            }
        }
    }
    // 局所最大を取る（θ ±4°、ρ ±8 の窓で抑制）。
    let mut peaks: Vec<(f32, usize, usize)> = Vec::new();
    let floor = acc.iter().copied().fold(0.0f32, f32::max) * 0.05;
    for t in 0..nt {
        for r in 0..nr {
            let v = acc[t * nr + r];
            if v <= floor {
                continue;
            }
            let mut is_max = true;
            'w: for dt in -4..=4isize {
                let tt = t as isize + dt;
                // θ は 180° で ρ の符号が反転して繋がる
                let (tt, flip) = if tt < 0 {
                    (tt + nt as isize, true)
                } else if tt >= nt as isize {
                    (tt - nt as isize, true)
                } else {
                    (tt, false)
                };
                for dr in -8..=8isize {
                    let rr = if flip {
                        2 * diag as isize - (r as isize) - dr
                    } else {
                        r as isize + dr
                    };
                    if rr < 0 || rr >= nr as isize || (dt == 0 && dr == 0) {
                        continue;
                    }
                    let o = acc[tt as usize * nr + rr as usize];
                    if o > v || (o == v && (tt as usize, rr as usize) < (t, r)) {
                        is_max = false;
                        break 'w;
                    }
                }
            }
            if is_max {
                peaks.push((v, t, r));
            }
        }
    }
    peaks.sort_by(|a, b| b.0.total_cmp(&a.0));
    peaks.truncate(24);
    peaks
        .into_iter()
        .map(|(_, t, r)| Line {
            theta: t as f64 * std::f64::consts::PI / nt as f64,
            rho: r as f64 - diag as f64,
        })
        .collect()
}

/// 辺 a→b に沿って、法線方向の勾配が続いている割合。
fn side_support(g: &Grad, a: Point, b: Point, thr: f32) -> f64 {
    let len = a.dist(b);
    if len < 4.0 {
        return 0.0;
    }
    let n = (len / 2.0).clamp(8.0, 200.0) as usize;
    let (dx, dy) = ((b.x - a.x) / len, (b.y - a.y) / len);
    let (nx, ny) = (-dy, dx);
    let mut hit = 0;
    for i in 0..n {
        let t = (i as f64 + 0.5) / n as f64;
        let p = Point::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t);
        let mut ok = false;
        for o in -2..=2 {
            let x = (p.x + nx * o as f64).floor() as isize;
            let y = (p.y + ny * o as f64).floor() as isize;
            if x < 0 || y < 0 || x >= g.w as isize || y >= g.h as isize {
                continue;
            }
            let i = y as usize * g.w + x as usize;
            if g.mag[i] >= thr {
                let c = ((g.gx[i] as f64 * nx + g.gy[i] as f64 * ny) / g.mag[i] as f64).abs();
                if c > 0.9 {
                    ok = true;
                    break;
                }
            }
        }
        if ok {
            hit += 1;
        }
    }
    hit as f64 / n as f64
}

fn polygon_area(q: &[Point; 4]) -> f64 {
    let mut s = 0.0;
    for i in 0..4 {
        let (a, b) = (q[i], q[(i + 1) % 4]);
        s += a.x * b.y - b.x * a.y;
    }
    s / 2.0
}

fn is_convex(q: &[Point; 4]) -> bool {
    let mut sign = 0.0;
    for i in 0..4 {
        let (a, b, c) = (q[i], q[(i + 1) % 4], q[(i + 2) % 4]);
        let z = (b.x - a.x) * (c.y - b.y) - (b.y - a.y) * (c.x - b.x);
        if z.abs() < 1e-9 {
            return false;
        }
        if sign == 0.0 {
            sign = z.signum();
        } else if z.signum() != sign {
            return false;
        }
    }
    true
}

/// 全解像度で 1 辺を当てはめ直す（法線方向に勾配の最大を探し、全最小二乗で直線を引く）。
fn refine_side(g: &Grad, a: Point, b: Point, search: f64) -> Option<(Point, Point)> {
    let len = a.dist(b);
    if len < 8.0 {
        return None;
    }
    let (dx, dy) = ((b.x - a.x) / len, (b.y - a.y) / len);
    let (nx, ny) = (-dy, dx);
    let n = (len / 4.0).clamp(16.0, 400.0) as usize;
    let mut pts: Vec<(f64, f64)> = Vec::new();
    let steps = (search * 4.0).ceil() as isize;
    for i in 0..n {
        // 角の近くは隣の辺の影響を受けるので端の 8% を避ける。
        let t = 0.08 + 0.84 * (i as f64 + 0.5) / n as f64;
        let p = Point::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t);
        let mut best = (0f32, 0.0f64);
        let sample = |o: f64| -> f32 {
            let x = p.x + nx * o - 0.5;
            let y = p.y + ny * o - 0.5;
            if x < 0.0 || y < 0.0 || x >= (g.w - 1) as f64 || y >= (g.h - 1) as f64 {
                return 0.0;
            }
            let (x0, y0) = (x.floor() as usize, y.floor() as usize);
            let (fx, fy) = ((x - x0 as f64) as f32, (y - y0 as f64) as f32);
            let at = |xx: usize, yy: usize| {
                let k = yy * g.w + xx;
                // 法線方向の成分だけ
                (g.gx[k] as f64 * nx + g.gy[k] as f64 * ny).abs() as f32
            };
            (at(x0, y0) * (1.0 - fx) + at(x0 + 1, y0) * fx) * (1.0 - fy)
                + (at(x0, y0 + 1) * (1.0 - fx) + at(x0 + 1, y0 + 1) * fx) * fy
        };
        for s in -steps..=steps {
            let o = s as f64 / 4.0;
            let v = sample(o);
            if v > best.0 {
                best = (v, o);
            }
        }
        if best.0 > 0.0 {
            // 放物線で小数位置を補間
            let (o, v0) = (best.1, best.0);
            let (vm, vp) = (sample(o - 0.25), sample(o + 0.25));
            let den = vm - 2.0 * v0 + vp;
            let off = if den.abs() > 1e-6 {
                (0.5 * (vm - vp) / den).clamp(-0.5, 0.5) as f64 * 0.25
            } else {
                0.0
            };
            pts.push((p.x + nx * (o + off), p.y + ny * (o + off)));
        }
    }
    let mut keep = pts.clone();
    let mut line = None;
    for _ in 0..3 {
        if keep.len() < 6 {
            return line;
        }
        let m = keep.len() as f64;
        let (mx, my) = (
            keep.iter().map(|p| p.0).sum::<f64>() / m,
            keep.iter().map(|p| p.1).sum::<f64>() / m,
        );
        let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
        for p in &keep {
            let (ex, ey) = (p.0 - mx, p.1 - my);
            sxx += ex * ex;
            sxy += ex * ey;
            syy += ey * ey;
        }
        // 主方向
        let ang = 0.5 * (2.0 * sxy).atan2(sxx - syy);
        let (ux, uy) = (ang.cos(), ang.sin());
        line = Some((Point::new(mx, my), Point::new(mx + ux, my + uy)));
        let dist = |p: &(f64, f64)| ((p.0 - mx) * -uy + (p.1 - my) * ux).abs();
        let mut ds: Vec<f64> = pts.iter().map(dist).collect();
        ds.sort_by(|a, b| a.total_cmp(b));
        let med = ds[ds.len() / 2];
        let lim = (3.0 * med).max(0.75);
        keep = pts.iter().copied().filter(|p| dist(p) <= lim).collect();
    }
    line
}

fn intersect_pp(a: (Point, Point), b: (Point, Point)) -> Option<Point> {
    let (p, r) = (a.0, Point::new(a.1.x - a.0.x, a.1.y - a.0.y));
    let (q, s) = (b.0, Point::new(b.1.x - b.0.x, b.1.y - b.0.y));
    let den = r.x * s.y - r.y * s.x;
    if den.abs() < 1e-9 {
        return None;
    }
    let t = ((q.x - p.x) * s.y - (q.y - p.y) * s.x) / den;
    Some(Point::new(p.x + t * r.x, p.y + t * r.y))
}

/// 板の四辺形を探す。見つからない・自信がないときは None。
pub fn detect_board(luma: &Plane, params: &DetectParams) -> Result<Option<DetectedBoard>> {
    let aw = params.analysis_width.clamp(64, 2048).min(luma.width());
    let scale = luma.width() as f64 / aw as f64;
    let ah = ((luma.height() as f64 / scale).round() as usize).max(16);
    let small = scale_plane(luma, aw, ah, ScaleFilter::Area)?;
    // 字などの細かい模様を弱め、板の縁（段差）を残すため強めにぼかす。
    let g = gradients(&small, 2.0);
    let mut mags: Vec<f32> = g.mag.clone();
    mags.sort_by(|a, b| a.total_cmp(b));
    let p99 = mags[mags.len() * 99 / 100];
    let thr = (0.15 * p99).max(6.0);
    let lines = hough(&g, thr);
    let (mut horiz, mut vert): (Vec<Line>, Vec<Line>) = (Vec::new(), Vec::new());
    for l in lines {
        // θ は法線の向き。法線が縦（θ ≈ 90°）なら横線。
        let deg = l.theta.to_degrees();
        if (45.0..135.0).contains(&deg) {
            horiz.push(l);
        } else {
            vert.push(l);
        }
    }
    horiz.truncate(8);
    vert.truncate(8);
    let (w, h) = (aw as f64, ah as f64);
    let margin = 0.05 * w.max(h);
    let mut best: Option<([Point; 4], f64, f64)> = None;
    for i in 0..horiz.len() {
        for j in i + 1..horiz.len() {
            for k in 0..vert.len() {
                for l in k + 1..vert.len() {
                    let (h1, h2, v1, v2) = (horiz[i], horiz[j], vert[k], vert[l]);
                    let corners = [
                        h1.intersect(&v1),
                        h1.intersect(&v2),
                        h2.intersect(&v2),
                        h2.intersect(&v1),
                    ];
                    let Some(mut q) = corners.into_iter().collect::<Option<Vec<Point>>>() else {
                        continue;
                    };
                    if q.iter().any(|p| {
                        p.x < -margin || p.y < -margin || p.x > w + margin || p.y > h + margin
                    }) {
                        continue;
                    }
                    // 左上・右上・右下・左下に並べる（重心からの角度で）。
                    let cx = q.iter().map(|p| p.x).sum::<f64>() / 4.0;
                    let cy = q.iter().map(|p| p.y).sum::<f64>() / 4.0;
                    q.sort_by(|a, b| {
                        (a.y - cy)
                            .atan2(a.x - cx)
                            .total_cmp(&(b.y - cy).atan2(b.x - cx))
                    });
                    // atan2 の順は -π（左）から: 左上(-135°)・右上(-45°)・右下(45°)・左下(135°)
                    let quad = [q[0], q[1], q[2], q[3]];
                    if !is_convex(&quad) {
                        continue;
                    }
                    let area = polygon_area(&quad).abs() / (w * h);
                    if area < params.min_area_fraction {
                        continue;
                    }
                    let sup: Vec<f64> = (0..4)
                        .map(|s| side_support(&g, quad[s], quad[(s + 1) % 4], thr * 0.5))
                        .collect();
                    let min_sup = sup.iter().copied().fold(1.0, f64::min);
                    let mean_sup = sup.iter().sum::<f64>() / 4.0;
                    let score = mean_sup * min_sup * area.powf(0.25);
                    if best.as_ref().is_none_or(|b| score > b.1) {
                        best = Some((quad, score, min_sup));
                    }
                }
            }
        }
    }
    let Some((quad, _, conf)) = best else {
        return Ok(None);
    };
    if conf < params.min_confidence {
        return Ok(None);
    }
    // 元の解像度で当てはめ直す。
    let full: [Point; 4] = quad.map(|p| Point::new(p.x * scale, p.y * scale));
    let gf = gradients(luma, (0.6 * scale as f32).clamp(0.8, 3.0));
    let search = 2.0 * scale + 2.0;
    let mut sides = Vec::new();
    for s in 0..4 {
        match refine_side(&gf, full[s], full[(s + 1) % 4], search) {
            Some(l) => sides.push(l),
            None => {
                return Ok(Some(DetectedBoard {
                    corners: full,
                    confidence: conf,
                }));
            }
        }
    }
    let mut corners = full;
    for (c, corner) in corners.iter_mut().enumerate() {
        // 角 c は辺 (c-1) と辺 c の交点
        if let Some(p) = intersect_pp(sides[(c + 3) % 4], sides[c])
            && p.dist(full[c]) <= 3.0 * search
        {
            *corner = p;
        }
    }
    Ok(Some(DetectedBoard {
        corners,
        confidence: conf,
    }))
}
