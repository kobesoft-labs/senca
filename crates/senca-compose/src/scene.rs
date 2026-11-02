//! シーンの記述（R8-1〜R8-3）: キャンバス、レイヤー（ソース・配置・切り抜き・重なり順・不透明度・角丸・枠）、
//! よく使う配置（PinP・左右に並べる・格子）。
//!
//! 値はすべて素朴な型にしてあり、設定（JSON など）へそのまま写せる。

use senca_image::{ScaleFilter, YuvColor, YuvSpec};

use crate::error::{ComposeError, Result};

/// 映像ソースの番号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub u32);

/// 画像（ロゴなど）の番号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageId(pub u32);

/// 出力の画面。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    pub fps_num: u32,
    pub fps_den: u32,
    /// 何もない所の色。
    pub background: YuvColor,
    /// 出力の YUV の規格（入力も同じ規格とみなす）。
    pub spec: YuvSpec,
}

impl Canvas {
    /// 1080p30・BT.709 限定範囲・黒地。
    pub fn hd1080() -> Self {
        Self::new(1920, 1080, 30, 1)
    }

    pub fn new(width: usize, height: usize, fps_num: u32, fps_den: u32) -> Self {
        Self {
            width,
            height,
            fps_num,
            fps_den,
            background: YuvColor::BLACK_LIMITED,
            spec: YuvSpec::BT709_LIMITED,
        }
    }

    /// n 番目の出力フレームの時刻（µs）。
    pub fn frame_time_us(&self, n: u64) -> i64 {
        let num = n as i128 * 1_000_000 * self.fps_den.max(1) as i128;
        (num / self.fps_num.max(1) as i128) as i64
    }

    pub(crate) fn validate(&self) -> Result<()> {
        senca_image::frame::check_dims(self.width, self.height)?;
        if !self.width.is_multiple_of(2) || !self.height.is_multiple_of(2) {
            return Err(ComposeError::InvalidConfig("キャンバスの幅・高さは偶数"));
        }
        if self.fps_num == 0 || self.fps_den == 0 {
            return Err(ComposeError::InvalidConfig("フレームレート"));
        }
        Ok(())
    }
}

/// キャンバス上の矩形（はみ出してよい）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DestRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl DestRect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// 元の映像の切り抜き（各辺から割合で削る。0〜1）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Crop {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// 縦横比の扱い。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FitMode {
    /// 矩形いっぱいに伸ばす。
    Stretch,
    /// 縦横比を保って収める（余白は透明）。
    #[default]
    Contain,
    /// 縦横比を保って覆う（はみ出しは切る）。
    Cover,
}

/// 枠線。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Border {
    pub width: u32,
    pub color: YuvColor,
}

/// レイヤーの中身。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LayerContent {
    /// ライブ入力。
    Video(SourceId),
    /// 画像（RGBA、アルファ付き）。
    Image(ImageId),
    /// 単色。
    Color(YuvColor),
}

/// 1 枚のレイヤー。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layer {
    pub content: LayerContent,
    pub dest: DestRect,
    pub crop: Crop,
    pub fit: FitMode,
    /// 大きいほど手前。同じなら並びの後ろが手前。
    pub z: i32,
    /// 0〜1。
    pub opacity: f32,
    /// 角の丸みの半径（画素）。
    pub corner_radius: u32,
    pub border: Option<Border>,
    pub visible: bool,
    pub filter: ScaleFilter,
}

impl Layer {
    /// 既定値（不透明・枠なし・縦横比を保つ・双三次）。
    pub fn new(content: LayerContent, dest: DestRect) -> Self {
        Self {
            content,
            dest,
            crop: Crop::default(),
            fit: FitMode::Contain,
            z: 0,
            opacity: 1.0,
            corner_radius: 0,
            border: None,
            visible: true,
            filter: ScaleFilter::Bicubic,
        }
    }
    pub fn video(src: SourceId, dest: DestRect) -> Self {
        Self::new(LayerContent::Video(src), dest)
    }
    pub fn with_z(mut self, z: i32) -> Self {
        self.z = z;
        self
    }
    pub fn with_opacity(mut self, o: f32) -> Self {
        self.opacity = o;
        self
    }
    pub fn with_fit(mut self, f: FitMode) -> Self {
        self.fit = f;
        self
    }
    pub fn with_border(mut self, width: u32, color: YuvColor) -> Self {
        self.border = Some(Border { width, color });
        self
    }
    pub fn with_corner_radius(mut self, r: u32) -> Self {
        self.corner_radius = r;
        self
    }
    pub fn with_crop(mut self, c: Crop) -> Self {
        self.crop = c;
        self
    }
}

/// シーン（レイヤーの集まり）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Scene {
    pub layers: Vec<Layer>,
}

/// レイヤーの大きさの上限（キャンバスの外へ大きくはみ出す指定を拒む）。
const MAX_LAYER_DIM: u32 = 16384;

impl Scene {
    pub fn new(layers: Vec<Layer>) -> Self {
        Self { layers }
    }

    /// 値の検査（適用する前に呼ぶ。仕様 5.1: 検証してから commit）。
    pub fn validate(&self) -> Result<()> {
        if self.layers.len() > 256 {
            return Err(ComposeError::InvalidConfig("レイヤーが多すぎる"));
        }
        for l in &self.layers {
            if !(0.0..=1.0).contains(&l.opacity) {
                return Err(ComposeError::InvalidConfig("不透明度は 0〜1"));
            }
            if l.dest.width > MAX_LAYER_DIM || l.dest.height > MAX_LAYER_DIM {
                return Err(ComposeError::InvalidConfig("レイヤーが大きすぎる"));
            }
            let c = l.crop;
            let ok = |v: f32| v.is_finite() && (0.0..1.0).contains(&v);
            if !(ok(c.left) && ok(c.right) && ok(c.top) && ok(c.bottom))
                || c.left + c.right >= 1.0
                || c.top + c.bottom >= 1.0
            {
                return Err(ComposeError::InvalidConfig("切り抜きの割合"));
            }
            if l.dest.x.unsigned_abs() > 4 * MAX_LAYER_DIM
                || l.dest.y.unsigned_abs() > 4 * MAX_LAYER_DIM
            {
                return Err(ComposeError::InvalidConfig("レイヤーの位置"));
            }
        }
        Ok(())
    }

    /// 描く順（奥から手前）のレイヤー。
    pub fn ordered(&self) -> Vec<&Layer> {
        let mut v: Vec<(usize, &Layer)> = self
            .layers
            .iter()
            .enumerate()
            .filter(|(_, l)| l.visible)
            .collect();
        v.sort_by_key(|(i, l)| (l.z, *i));
        v.into_iter().map(|(_, l)| l).collect()
    }
}

/// PinP の子画面の位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    #[default]
    BottomRight,
}

/// 配置の補助。
pub mod layout {
    use super::*;

    fn even(v: f64) -> u32 {
        ((v.max(2.0) as u32) / 2) * 2
    }

    /// 全面に `main`、隅に `inset`（キャンバスの幅の `scale` 倍、余白 `margin`）。
    pub fn pinp(
        canvas: &Canvas,
        main: SourceId,
        inset: SourceId,
        corner: Corner,
        scale: f32,
        margin: u32,
    ) -> Scene {
        let (cw, ch) = (canvas.width as f64, canvas.height as f64);
        let w = even(cw * scale.clamp(0.05, 1.0) as f64);
        let h = even(w as f64 * ch / cw);
        let m = margin as i32;
        let (x, y) = match corner {
            Corner::TopLeft => (m, m),
            Corner::TopRight => (cw as i32 - w as i32 - m, m),
            Corner::BottomLeft => (m, ch as i32 - h as i32 - m),
            Corner::BottomRight => (cw as i32 - w as i32 - m, ch as i32 - h as i32 - m),
        };
        Scene::new(vec![
            Layer::video(main, DestRect::new(0, 0, cw as u32, ch as u32)),
            Layer::video(inset, DestRect::new(x & !1, y & !1, w, h))
                .with_z(1)
                .with_border(4, YuvColor::WHITE_LIMITED),
        ])
    }

    /// 左右に 2 つ並べる（間 `gap`、縦横比を保って中央に）。
    pub fn side_by_side(canvas: &Canvas, left: SourceId, right: SourceId, gap: u32) -> Scene {
        grid_with(canvas, &[left, right], 2, 1, gap)
    }

    /// 格子に並べる（列数は ceil(sqrt(n))）。
    pub fn grid(canvas: &Canvas, sources: &[SourceId], gap: u32) -> Scene {
        let n = sources.len().max(1);
        let cols = (n as f64).sqrt().ceil() as usize;
        let rows = n.div_ceil(cols);
        grid_with(canvas, sources, cols, rows, gap)
    }

    fn grid_with(
        canvas: &Canvas,
        sources: &[SourceId],
        cols: usize,
        rows: usize,
        gap: u32,
    ) -> Scene {
        let g = gap as f64;
        let cell_w = (canvas.width as f64 - g * (cols as f64 + 1.0)) / cols as f64;
        let cell_h = (canvas.height as f64 - g * (rows as f64 + 1.0)) / rows as f64;
        let layers = sources
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                let (c, r) = (i % cols, i / cols);
                let x = g + c as f64 * (cell_w + g);
                let y = g + r as f64 * (cell_h + g);
                Layer::video(
                    s,
                    DestRect::new((x as i32) & !1, (y as i32) & !1, even(cell_w), even(cell_h)),
                )
            })
            .collect();
        Scene::new(layers)
    }
}
