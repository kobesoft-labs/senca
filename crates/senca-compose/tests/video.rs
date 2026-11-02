//! 合成の画素試験。

use std::sync::Arc;

use senca_compose::{
    Canvas, Compositor, Corner, DestRect, FitMode, ImageId, Layer, LayerContent, OverlayImage,
    Scene, Slate, SourceId, SourcePolicy, Transition, TransitionKind, layout,
};
use senca_image::{I420Frame, RgbaImage, YuvColor, YuvSpec};

const RED: YuvColor = YuvColor::new(63, 102, 240);
const BLUE: YuvColor = YuvColor::new(32, 240, 118);
const GREEN: YuvColor = YuvColor::new(173, 42, 26);

fn solid(w: usize, h: usize, c: YuvColor) -> Arc<I420Frame> {
    Arc::new(I420Frame::new(w, h, c).unwrap())
}

fn px(f: &I420Frame, x: usize, y: usize) -> (u8, u8, u8) {
    (
        f.y().get(x, y),
        f.u().get(x / 2, y / 2),
        f.v().get(x / 2, y / 2),
    )
}

fn canvas() -> Canvas {
    Canvas::new(320, 180, 30, 1)
}

fn pattern(w: usize, h: usize) -> Arc<I420Frame> {
    let mut f = I420Frame::new(w, h, YuvColor::new(128, 128, 128)).unwrap();
    for y in 0..h {
        for x in 0..w {
            f.y_mut().set(x, y, ((x * 7 + y * 3) % 220 + 16) as u8);
        }
    }
    Arc::new(f)
}

#[test]
fn identity_layer_is_exact() {
    let mut c = Compositor::new(canvas(), 4).unwrap();
    let src = pattern(320, 180);
    c.push_frame(SourceId(1), src.clone(), 0);
    c.set_program(Scene::new(vec![Layer::video(
        SourceId(1),
        DestRect::new(0, 0, 320, 180),
    )]))
    .unwrap();
    let out = c.render(0).unwrap();
    assert_eq!(&out, src.as_ref());
}

#[test]
fn z_order_and_opacity() {
    let mut c = Compositor::new(canvas(), 2).unwrap();
    let s = Scene::new(vec![
        Layer::new(LayerContent::Color(RED), DestRect::new(0, 0, 200, 180)).with_z(1),
        Layer::new(LayerContent::Color(BLUE), DestRect::new(100, 0, 220, 180)).with_z(0),
        Layer::new(
            LayerContent::Color(YuvColor::new(200, 128, 128)),
            DestRect::new(0, 100, 320, 80),
        )
        .with_z(2)
        .with_opacity(0.5),
    ]);
    c.set_program(s).unwrap();
    let out = c.render(0).unwrap();
    assert_eq!(px(&out, 150, 50), (RED.y, RED.u, RED.v), "赤が手前");
    assert_eq!(px(&out, 250, 50), (BLUE.y, BLUE.u, BLUE.v));
    // 半透明: (63 + 200) / 2
    let (y, _, _) = px(&out, 50, 150);
    assert!((y as i32 - 132).abs() <= 1, "{y}");
}

#[test]
fn pinp_layout() {
    let cv = canvas();
    let mut c = Compositor::new(cv, 3).unwrap();
    c.push_frame(SourceId(1), solid(640, 360, RED), 0);
    c.push_frame(SourceId(2), solid(160, 90, BLUE), 0);
    let s = layout::pinp(&cv, SourceId(1), SourceId(2), Corner::BottomRight, 0.25, 10);
    c.set_program(s.clone()).unwrap();
    let out = c.render(0).unwrap();
    let inset = s.layers[1].dest;
    let (cx, cy) = (
        (inset.x + inset.width as i32 / 2) as usize,
        (inset.y + inset.height as i32 / 2) as usize,
    );
    assert_eq!(px(&out, cx, cy), (BLUE.y, BLUE.u, BLUE.v));
    assert_eq!(px(&out, 20, 20), (RED.y, RED.u, RED.v));
    // 枠（白）
    assert_eq!(out.y().get(inset.x as usize + 1, cy), 235);
}

#[test]
fn grid_and_side_by_side() {
    let cv = canvas();
    let mut c = Compositor::new(cv, 4).unwrap();
    let cols = [RED, BLUE, GREEN, YuvColor::new(200, 128, 128)];
    for (i, col) in cols.iter().enumerate() {
        c.push_frame(SourceId(i as u32), solid(320, 180, *col), 0);
    }
    let ids: Vec<SourceId> = (0..4).map(SourceId).collect();
    let g = layout::grid(&cv, &ids, 4);
    c.set_program(g.clone()).unwrap();
    let out = c.render(0).unwrap();
    for (l, col) in g.layers.iter().zip(cols) {
        let (x, y) = (
            (l.dest.x + l.dest.width as i32 / 2) as usize,
            (l.dest.y + l.dest.height as i32 / 2) as usize,
        );
        assert_eq!(px(&out, x, y).0, col.y);
    }
    let sbs = layout::side_by_side(&cv, SourceId(0), SourceId(1), 0);
    c.set_program(sbs).unwrap();
    let out = c.render(0).unwrap();
    assert_eq!(px(&out, 80, 90).0, RED.y);
    assert_eq!(px(&out, 240, 90).0, BLUE.y);
    // 縦横比を保つので上下に余白（黒）
    assert_eq!(px(&out, 80, 5).0, 16);
}

#[test]
fn hold_last_frame_and_slate() {
    let mut c = Compositor::new(canvas(), 2).unwrap();
    c.set_policy(SourcePolicy {
        hold_timeout_us: 500_000,
        slate: Slate::Color(GREEN),
    });
    c.set_program(Scene::new(vec![Layer::video(
        SourceId(7),
        DestRect::new(0, 0, 320, 180),
    )]))
    .unwrap();
    c.push_frame(SourceId(7), solid(320, 180, RED), 0);
    c.push_frame(SourceId(7), solid(320, 180, BLUE), 100_000);
    assert_eq!(
        px(&c.render(50_000).unwrap(), 10, 10).0,
        RED.y,
        "時刻 0.05 s は前のフレーム"
    );
    assert_eq!(px(&c.render(400_000).unwrap(), 10, 10).0, BLUE.y);
    assert!(c.last_missing().is_empty());
    // 0.5 s を超えたらスレート
    assert_eq!(px(&c.render(700_000).unwrap(), 10, 10).0, GREEN.y);
    assert_eq!(c.last_missing(), &[SourceId(7)]);
    // 切れたと知らされたら直ちに
    c.push_frame(SourceId(7), solid(320, 180, RED), 800_000);
    c.mark_missing(SourceId(7));
    assert_eq!(px(&c.render(800_000).unwrap(), 10, 10).0, GREEN.y);
    // スレートを画像に
    let mut img = RgbaImage::new(4, 4).unwrap();
    img.data_mut().fill(255);
    c.set_image(
        ImageId(1),
        OverlayImage::from_rgba(&img, YuvSpec::BT709_LIMITED),
    );
    c.set_policy(SourcePolicy {
        hold_timeout_us: 500_000,
        slate: Slate::Image(ImageId(1)),
    });
    assert_eq!(px(&c.render(900_000).unwrap(), 160, 90).0, 235);
}

#[test]
fn transitions() {
    let mut c = Compositor::new(canvas(), 4).unwrap();
    let full = DestRect::new(0, 0, 320, 180);
    let a = Scene::new(vec![Layer::new(
        LayerContent::Color(YuvColor::new(40, 128, 128)),
        full,
    )]);
    let b = Scene::new(vec![Layer::new(
        LayerContent::Color(YuvColor::new(200, 128, 128)),
        full,
    )]);
    c.set_program(a.clone()).unwrap();
    c.transition_to(
        b.clone(),
        Transition {
            kind: TransitionKind::Fade,
            duration_ms: 1000,
        },
        0,
    )
    .unwrap();
    assert!(c.in_transition());
    let mid = c.render(500_000).unwrap();
    assert!((mid.y().get(100, 100) as i32 - 120).abs() <= 1);
    let q = c.render(250_000).unwrap();
    assert!((q.y().get(100, 100) as i32 - 80).abs() <= 1);
    let end = c.render(1_000_000).unwrap();
    assert_eq!(end.y().get(100, 100), 200);
    assert!(!c.in_transition());

    c.transition_to(
        a.clone(),
        Transition {
            kind: TransitionKind::WipeLeftToRight,
            duration_ms: 400,
        },
        2_000_000,
    )
    .unwrap();
    let w = c.render(2_100_000).unwrap();
    assert_eq!(w.y().get(10, 50), 40);
    assert_eq!(w.y().get(100, 50), 200);
    c.transition_to(
        b,
        Transition {
            kind: TransitionKind::WipeRightToLeft,
            duration_ms: 400,
        },
        3_000_000,
    )
    .unwrap();
    let w = c.render(3_100_000).unwrap();
    assert_eq!(w.y().get(10, 50), 40);
    assert_eq!(w.y().get(310, 50), 200);
    c.transition_to(
        a,
        Transition {
            kind: TransitionKind::Cut,
            duration_ms: 400,
        },
        4_000_000,
    )
    .unwrap();
    assert_eq!(c.render(4_000_000).unwrap().y().get(10, 50), 40);
}

#[test]
fn alpha_image_and_rounded_corners() {
    let mut c = Compositor::new(canvas(), 2).unwrap();
    // 左半分が不透明な白、右半分が透明
    let mut img = RgbaImage::new(40, 20).unwrap();
    for y in 0..20 {
        for x in 0..40 {
            let a = if x < 20 { 255 } else { 0 };
            img.pixel_mut(x, y).copy_from_slice(&[255, 255, 255, a]);
        }
    }
    c.set_image(
        ImageId(3),
        OverlayImage::from_rgba(&img, YuvSpec::BT709_LIMITED),
    );
    c.push_frame(SourceId(1), solid(320, 180, BLUE), 0);
    let s = Scene::new(vec![
        Layer::new(LayerContent::Color(RED), DestRect::new(0, 0, 320, 180)),
        Layer::video(SourceId(1), DestRect::new(100, 40, 120, 100))
            .with_fit(FitMode::Stretch)
            .with_corner_radius(20)
            .with_z(1),
        Layer::new(LayerContent::Image(ImageId(3)), DestRect::new(0, 0, 80, 40))
            .with_fit(FitMode::Stretch)
            .with_z(2),
    ]);
    c.set_program(s).unwrap();
    let out = c.render(0).unwrap();
    assert_eq!(out.y().get(10, 10), 235, "画像の不透明部分");
    assert_eq!(out.y().get(70, 10), RED.y, "画像の透明部分");
    assert_eq!(out.y().get(101, 41), RED.y, "角丸の外");
    assert_eq!(out.y().get(160, 90), BLUE.y, "中央");
    assert_eq!(out.y().get(101, 90), BLUE.y, "辺の中ほどは内側");
}

#[test]
fn multithread_equals_single_thread() {
    let cv = Canvas::new(640, 360, 30, 1);
    let scene = layout::pinp(&cv, SourceId(1), SourceId(2), Corner::TopLeft, 0.3, 8);
    let mut s2 = scene.clone();
    s2.layers[1].corner_radius = 12;
    s2.layers.push(
        Layer::new(
            LayerContent::Color(GREEN),
            DestRect::new(-30, 300, 200, 100),
        )
        .with_opacity(0.4)
        .with_z(5),
    );
    let mut outs = Vec::new();
    for threads in [1, 3, 8] {
        let mut c = Compositor::new(cv, threads).unwrap();
        c.push_frame(SourceId(1), pattern(1280, 720), 0);
        c.push_frame(SourceId(2), pattern(333, 211), 0);
        c.set_program(s2.clone()).unwrap();
        outs.push(c.render(0).unwrap());
    }
    assert_eq!(outs[0], outs[1]);
    assert_eq!(outs[0], outs[2]);
}

#[test]
fn rejects_bad_scenes() {
    let mut c = Compositor::new(canvas(), 1).unwrap();
    let bad = Scene::new(vec![
        Layer::new(LayerContent::Color(RED), DestRect::new(0, 0, 10, 10)).with_opacity(1.5),
    ]);
    assert!(c.set_program(bad).is_err());
    let bad = Scene::new(vec![Layer::new(
        LayerContent::Color(RED),
        DestRect::new(0, 0, 100_000, 10),
    )]);
    assert!(c.set_program(bad).is_err());
    assert!(Compositor::new(Canvas::new(321, 180, 30, 1), 1).is_err());
    assert!(Compositor::new(Canvas::new(320, 180, 0, 1), 1).is_err());
}

/// 1080p の合成の時間（1 スレッドと複数スレッド）。`--ignored --nocapture` で表示する。
#[test]
#[ignore = "性能の計測"]
fn bench_compose_1080p() {
    let cv = Canvas::hd1080();
    let mut scene = layout::pinp(&cv, SourceId(1), SourceId(2), Corner::BottomRight, 0.3, 32);
    scene.layers[1].corner_radius = 24;
    scene.layers.push(
        Layer::video(SourceId(3), DestRect::new(40, 40, 480, 270))
            .with_z(3)
            .with_opacity(0.8),
    );
    let n_cpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let mut results = Vec::new();
    for threads in [1, n_cpu] {
        let mut c = Compositor::new(cv, threads).unwrap();
        c.push_frame(SourceId(1), pattern(1920, 1080), 0);
        c.push_frame(SourceId(2), pattern(1280, 720), 0);
        c.push_frame(SourceId(3), pattern(1920, 1080), 0);
        c.set_program(scene.clone()).unwrap();
        c.render(0).unwrap();
        let t = std::time::Instant::now();
        let n = 20;
        for i in 0..n {
            std::hint::black_box(c.render(i).unwrap());
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        println!("1080p 合成（3 レイヤー）: {threads} スレッド {ms:.2} ms/フレーム");
        results.push(ms);
    }
    if n_cpu >= 4 {
        println!("速度の比 {:.2} 倍", results[0] / results[1]);
        assert!(results[1] < results[0], "並列で速くならない");
    }
}
