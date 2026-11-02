//! 合成画像による板の補正の試験。
//!
//! 板の正解画像（字・色マーカー・目印の四角）を作り、斜めから撮ったカメラ画像
//! （壁・照明の傾き・反射の斑点つき）を描いてから、補正で元に戻るかを確かめる。

use senca_board::homography::{Homography, Point, bilinear, rect_corners, warp_plane};
use senca_board::{
    BoardChangeDetector, BoardCorrector, BoardKind, BoardParams, ChangeParams, DetectParams,
    EnhanceParams, detect_board, enhance_i420, warp_i420,
};
use senca_image::{I420Frame, Plane, YuvColor, YuvSpec};

const BW: usize = 640;
const BH: usize = 360;
const CW: usize = 960;
const CH: usize = 540;
const QUAD: [Point; 4] = [
    Point::new(150.0, 90.0),
    Point::new(820.0, 60.0),
    Point::new(860.0, 470.0),
    Point::new(120.0, 440.0),
];
const MARKERS: [(f64, f64); 4] = [(40.0, 40.0), (600.0, 40.0), (600.0, 320.0), (40.0, 320.0)];

struct Board {
    frame: I420Frame,
    /// 字（または目印）の画素なら真。
    ink: Vec<bool>,
}

fn rect(
    f: &mut I420Frame,
    ink: &mut [bool],
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    c: YuvColor,
) {
    for y in y0..(y0 + h).min(BH) {
        for x in x0..(x0 + w).min(BW) {
            f.y_mut().set(x, y, c.y);
            ink[y * BW + x] = true;
        }
    }
    for y in y0 / 2..((y0 + h) / 2).min(BH / 2) {
        for x in x0 / 2..((x0 + w) / 2).min(BW / 2) {
            f.u_mut().set(x, y, c.u);
            f.v_mut().set(x, y, c.v);
        }
    }
}

/// 板の正解。`extra` が真なら字を書き足した版。
fn board(kind: BoardKind, extra: bool) -> Board {
    let spec = YuvSpec::BT709_LIMITED;
    let (bg, ink_c, red, blue) = match kind {
        BoardKind::Whiteboard => (
            spec.rgb_to_yuv(235, 235, 230),
            spec.rgb_to_yuv(30, 30, 35),
            spec.rgb_to_yuv(200, 30, 30),
            spec.rgb_to_yuv(30, 50, 200),
        ),
        BoardKind::Blackboard => (
            spec.rgb_to_yuv(40, 70, 55),
            spec.rgb_to_yuv(235, 235, 235),
            spec.rgb_to_yuv(240, 220, 60),
            spec.rgb_to_yuv(120, 170, 240),
        ),
    };
    let mut f = I420Frame::new(BW, BH, bg).unwrap();
    let mut ink = vec![false; BW * BH];
    // 文字のような横線・縦線
    for line in 0..5 {
        let y = 80 + line * 45;
        let mut x = 80;
        let mut s = 7u32 + line as u32;
        while x < 540 {
            s = s.wrapping_mul(1103515245).wrapping_add(12345);
            let w = 8 + (s >> 16) as usize % 20;
            let c = if line == 2 {
                red
            } else if line == 4 {
                blue
            } else {
                ink_c
            };
            rect(&mut f, &mut ink, x, y, w, 4, c);
            rect(&mut f, &mut ink, x, y, 4, 22, c);
            x += w + 10;
        }
    }
    if extra {
        for k in 0..6 {
            rect(&mut f, &mut ink, 100 + k * 70, 300, 40, 5, ink_c);
        }
    }
    for &(mx, my) in &MARKERS {
        rect(
            &mut f,
            &mut ink,
            mx as usize - 3,
            my as usize - 3,
            6,
            6,
            ink_c,
        );
    }
    Board { frame: f, ink }
}

/// カメラ画像: 壁（中間の灰色）に板を斜めに描き、照明の傾きと反射を足す。
fn camera(b: &Board, kind: BoardKind) -> I420Frame {
    let cam_to_board = Homography::from_points(&QUAD, &rect_corners(BW, BH)).unwrap();
    let wall = YuvColor::new(110, 128, 128);
    let mut f = I420Frame::new(CW, CH, wall).unwrap();
    let (glare_amp, glare_c) = match kind {
        BoardKind::Whiteboard => (90.0, (560.0, 220.0)),
        BoardKind::Blackboard => (110.0, (560.0, 220.0)),
    };
    let render = |src: &Plane, dst: &mut Plane, scale: f64, is_luma: bool| {
        for y in 0..dst.height() {
            for x in 0..dst.width() {
                let mut acc = 0.0;
                let mut n = 0;
                let mut inside = 0;
                for sy in 0..2 {
                    for sx in 0..2 {
                        let p = Point::new(
                            (x as f64 + 0.25 + 0.5 * sx as f64) / scale,
                            (y as f64 + 0.25 + 0.5 * sy as f64) / scale,
                        );
                        let q = cam_to_board.apply(p).unwrap();
                        let q = Point::new(q.x * scale, q.y * scale);
                        n += 1;
                        if q.x >= 0.0
                            && q.y >= 0.0
                            && q.x < src.width() as f64
                            && q.y < src.height() as f64
                        {
                            inside += 1;
                            acc += bilinear(src, q.x - 0.5, q.y - 0.5);
                        } else {
                            acc += dst.get(x, y) as f64;
                        }
                    }
                }
                let mut v = acc / n as f64;
                if is_luma && inside > 0 {
                    // 照明: 左が暗い
                    let cx = x as f64 / CW as f64;
                    let gain = 0.62 + 0.38 * cx;
                    v = 16.0 + (v - 16.0) * gain;
                    let d2 = (x as f64 - glare_c.0).powi(2) + (y as f64 - glare_c.1).powi(2);
                    v += glare_amp * (-d2 / (2.0 * 45.0f64.powi(2))).exp();
                }
                dst.set(x, y, v.round().clamp(0.0, 255.0) as u8);
            }
        }
    };
    let (yy, uu, vv) = f.planes_mut();
    render(b.frame.y(), yy, 1.0, true);
    render(b.frame.u(), uu, 0.5, false);
    render(b.frame.v(), vv, 0.5, false);
    f
}

/// 目印の四角の重心（暗い／明るい画素の重み付き）。
fn marker_centroid(p: &Plane, cx: f64, cy: f64, dark: bool) -> (f64, f64) {
    let (mut sx, mut sy, mut sw) = (0.0, 0.0, 0.0);
    let bgv = p.get(cx as usize + 9, cy as usize) as f64;
    for y in (cy as usize - 8)..(cy as usize + 8) {
        for x in (cx as usize - 8)..(cx as usize + 8) {
            let v = p.get(x, y) as f64;
            let w = if dark {
                (bgv - v).max(0.0)
            } else {
                (v - bgv).max(0.0)
            };
            sx += w * (x as f64 + 0.5);
            sy += w * (y as f64 + 0.5);
            sw += w;
        }
    }
    (sx / sw, sy / sw)
}

#[test]
fn keystone_recovers_marker_positions() {
    let b = board(BoardKind::Whiteboard, false);
    let cam = camera(&b, BoardKind::Whiteboard);
    let out = warp_i420(&cam, &QUAD, BW, BH, YuvColor::BLACK_LIMITED).unwrap();
    for &(mx, my) in &MARKERS {
        let (x, y) = marker_centroid(out.y(), mx, my, true);
        let err = ((x - mx).powi(2) + (y - my).powi(2)).sqrt();
        println!("目印の誤差 {err:.3} px");
        assert!(
            err < 1.0,
            "目印 ({mx},{my}) → ({x:.2},{y:.2}) 誤差 {err:.2}"
        );
    }
    // 正解と同じ照明にしてから比べなくても、構造はかなり一致する（相関で確認）。
    let a = b.frame.y();
    let c = out.y();
    let n = (BW * BH) as f64;
    let (ma, mc) = (
        a.to_packed().iter().map(|&v| v as f64).sum::<f64>() / n,
        c.to_packed().iter().map(|&v| v as f64).sum::<f64>() / n,
    );
    let (mut cov, mut va, mut vc) = (0.0, 0.0, 0.0);
    for y in 0..BH {
        for x in 0..BW {
            let (p, q) = (a.get(x, y) as f64 - ma, c.get(x, y) as f64 - mc);
            cov += p * q;
            va += p * p;
            vc += q * q;
        }
    }
    let corr = cov / (va * vc).sqrt();
    // 照明の傾き・反射があるので 1 にはならない。
    assert!(corr > 0.7, "相関 {corr}");
}

#[test]
fn auto_detect_finds_board_corners() {
    let b = board(BoardKind::Whiteboard, false);
    let cam = camera(&b, BoardKind::Whiteboard);
    let d = detect_board(cam.y(), &DetectParams::default())
        .unwrap()
        .expect("板が見つからない");
    for (got, want) in d.corners.iter().zip(QUAD.iter()) {
        println!("角の誤差 {:.2} px", got.dist(*want));
        assert!(
            got.dist(*want) < 2.5,
            "角 {got:?} 正解 {want:?}（信頼度 {:.2}）",
            d.confidence
        );
    }
    assert!(d.confidence > 0.6);
    let bb = board(BoardKind::Blackboard, false);
    let cam = camera(&bb, BoardKind::Blackboard);
    let d = detect_board(cam.y(), &DetectParams::default())
        .unwrap()
        .expect("黒板が見つからない");
    for (got, want) in d.corners.iter().zip(QUAD.iter()) {
        println!("黒板の角の誤差 {:.2} px", got.dist(*want));
        assert!(got.dist(*want) < 2.5);
    }
    // 何もない画像では None
    let flat = Plane::filled(CW, CH, 120).unwrap();
    assert!(
        detect_board(&flat, &DetectParams::default())
            .unwrap()
            .is_none()
    );
}

/// 地の画素（字から 6 画素以上離れ、反射の中心から離れた所）の輝度の標準偏差と、字と地の平均の差。
fn stats(y: &Plane, ink: &[bool], kind: BoardKind) -> (f64, f64) {
    let dil = |x: usize, yy: usize| {
        for dy in -6..=6isize {
            for dx in -6..=6isize {
                let (xx, y2) = (x as isize + dx, yy as isize + dy);
                if xx >= 0
                    && y2 >= 0
                    && (xx as usize) < BW
                    && (y2 as usize) < BH
                    && ink[y2 as usize * BW + xx as usize]
                {
                    return true;
                }
            }
        }
        false
    };
    let (mut bs, mut bs2, mut bn) = (0.0, 0.0, 0.0);
    let (mut is, mut inn) = (0.0, 0.0);
    for yy in 8..BH - 8 {
        for x in 8..BW - 8 {
            let v = y.get(x, yy) as f64;
            if ink[yy * BW + x] {
                is += v;
                inn += 1.0;
            } else if !dil(x, yy) {
                bs += v;
                bs2 += v * v;
                bn += 1.0;
            }
        }
    }
    let bm = bs / bn;
    let std = (bs2 / bn - bm * bm).sqrt();
    let contrast = match kind {
        BoardKind::Whiteboard => bm - is / inn,
        BoardKind::Blackboard => is / inn - bm,
    };
    (std, contrast)
}

fn check_enhance(kind: BoardKind) {
    let b = board(kind, false);
    let cam = camera(&b, kind);
    let warped = warp_i420(&cam, &QUAD, BW, BH, YuvColor::BLACK_LIMITED).unwrap();
    let params = match kind {
        BoardKind::Whiteboard => EnhanceParams::whiteboard(),
        BoardKind::Blackboard => EnhanceParams::blackboard(),
    };
    let out = enhance_i420(
        &warped,
        &EnhanceParams {
            max_stroke_width: 26,
            ..params
        },
    )
    .unwrap();
    let (s0, c0) = stats(warped.y(), &b.ink, kind);
    let (s1, c1) = stats(out.y(), &b.ink, kind);
    println!("{kind:?}: 地の標準偏差 {s0:.2} → {s1:.2}、字のコントラスト {c0:.1} → {c1:.1}");
    assert!(s1 < s0 * 0.3, "{kind:?}: 地の平坦さ {s0} → {s1}");
    assert!(c1 > c0 * 1.2, "{kind:?}: コントラスト {c0} → {c1}");
    // 色マーカー（3 行目、赤／黄）の色が残る
    let mut found = 0;
    for x in 40..270 {
        let (u, v) = (out.u().get(x, 86), out.v().get(x, 86));
        let colored = match kind {
            BoardKind::Whiteboard => v > 150,             // 赤
            BoardKind::Blackboard => u < 110 && v >= 128, // 黄
        };
        if colored {
            found += 1;
        }
    }
    assert!(found > 20, "{kind:?}: 色の字が残っていない ({found})");
}

#[test]
fn whiteboard_illumination_and_glare_corrected() {
    check_enhance(BoardKind::Whiteboard);
}

#[test]
fn blackboard_chalk_enhanced() {
    check_enhance(BoardKind::Blackboard);
}

#[test]
fn blackboard_invert_to_white() {
    let b = board(BoardKind::Blackboard, false);
    let p = EnhanceParams {
        invert_to_white: true,
        max_stroke_width: 26,
        ..EnhanceParams::blackboard()
    };
    let out = enhance_i420(&b.frame, &p).unwrap();
    let (_, contrast) = stats(out.y(), &b.ink, BoardKind::Whiteboard);
    assert!(contrast > 100.0, "{contrast}");
    assert!(out.y().get(20, 200) > 225);
}

#[test]
fn corrector_pipeline() {
    let b = board(BoardKind::Whiteboard, false);
    let cam = camera(&b, BoardKind::Whiteboard);
    let c = BoardCorrector::new(BoardParams {
        corners: Some(QUAD),
        output_width: BW,
        output_height: BH,
        ..Default::default()
    })
    .unwrap();
    let out = c.process(&cam).unwrap();
    assert_eq!((out.width(), out.height()), (BW, BH));
    let bad = [Point::new(0.0, 0.0); 4];
    assert!(
        BoardCorrector::new(BoardParams {
            corners: Some(bad),
            ..Default::default()
        })
        .is_err()
    );
}

#[test]
fn change_detector_ignores_passerby_and_fires_on_new_content() {
    let a = board(BoardKind::Whiteboard, false);
    let b = board(BoardKind::Whiteboard, true);
    let mut det = BoardChangeDetector::new(ChangeParams::default());
    let mut events = Vec::new();
    let mut t = 0;
    let feed =
        |det: &mut BoardChangeDetector, f: &Plane, events: &mut Vec<usize>, t: &mut usize| {
            let r = det.push(f).unwrap();
            if r.snapshot {
                events.push(*t);
            }
            *t += 1;
            r
        };
    for _ in 0..12 {
        feed(&mut det, a.frame.y(), &mut events, &mut t);
    }
    assert_eq!(events.len(), 1, "最初の板書で 1 回: {events:?}");
    // 人（暗い大きな矩形）が横切る
    for k in 0..10 {
        let mut f = a.frame.y().clone();
        let x0 = 40 + k * 50;
        for y in 60..BH {
            for x in x0..(x0 + 120).min(BW) {
                f.set(x, y, 60);
            }
        }
        let r = feed(&mut det, &f, &mut events, &mut t);
        assert!(!r.snapshot, "横切りで発火した t={t} {r:?}");
    }
    for _ in 0..4 {
        feed(&mut det, a.frame.y(), &mut events, &mut t);
    }
    assert_eq!(events.len(), 1, "横切りで発火しない: {events:?}");
    // 書き足す
    let before = t;
    for _ in 0..12 {
        feed(&mut det, b.frame.y(), &mut events, &mut t);
    }
    assert_eq!(events.len(), 2, "書き足しで 1 回: {events:?}");
    assert!(events[1] - before <= 10);
    // 消す（元に戻る）と 1 回
    for _ in 0..12 {
        let r = feed(&mut det, a.frame.y(), &mut events, &mut t);
        if r.snapshot {
            assert!(r.removed_fraction > r.added_fraction);
        }
    }
    assert_eq!(events.len(), 3);
}

#[test]
fn warp_plane_handles_out_of_range() {
    let p = Plane::filled(10, 10, 200).unwrap();
    let h = Homography::from_points(
        &rect_corners(10, 10),
        &[
            Point::new(-100.0, -100.0),
            Point::new(-90.0, -100.0),
            Point::new(-90.0, -90.0),
            Point::new(-100.0, -90.0),
        ],
    )
    .unwrap();
    let q = warp_plane(&p, &h, 10, 10, 7).unwrap();
    assert!(q.to_packed().iter().all(|&v| v == 7));
}
