//! 黒板・ホワイトボードの補正（REQ-006 R6-5 / R6-6 / R6-8）。
//!
//! - [`homography`] — 4 点の対応からの射影変換（正規化 DLT）と台形補正（透視変換）
//! - [`detect`] — 板の四辺形の自動検出（ベストエフォート、自信がなければ None）
//! - [`enhance`] — 照明むら・反射の補正、CLAHE、ホワイトボードの白地化、黒板のチョークの強調
//! - [`change`] — 板書の変化検出（静止画の自動保存の合図）
//!
//! 入出力は I420（輝度だけの補助関数もある）。画像の部品は `senca-image`（自前）を使う。

pub mod change;
pub mod detect;
pub mod enhance;
pub mod homography;

pub use change::{BoardChangeDetector, ChangeParams, ChangeReport};
pub use detect::{DetectParams, DetectedBoard, detect_board};
pub use enhance::{BoardKind, ClaheParams, EnhanceParams, enhance_gray, enhance_i420};
pub use homography::{Homography, Point, warp_i420, warp_plane};

use senca_image::{I420Frame, ImageError, Result};

/// 板の補正の設定（台形補正＋補正）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoardParams {
    /// 入力の板の 4 隅（左上・右上・右下・左下、輝度の座標）。None なら台形補正をしない。
    pub corners: Option<[Point; 4]>,
    /// 出力の大きさ（台形補正するとき）。
    pub output_width: usize,
    pub output_height: usize,
    pub enhance: EnhanceParams,
    /// 照明などの補正をする（偽なら台形補正だけ）。
    pub enhance_enabled: bool,
}

impl Default for BoardParams {
    fn default() -> Self {
        Self {
            corners: None,
            output_width: 1920,
            output_height: 1080,
            enhance: EnhanceParams::default(),
            enhance_enabled: true,
        }
    }
}

/// 板の補正: 台形補正 → 照明・反射の補正と強調。
#[derive(Debug, Clone)]
pub struct BoardCorrector {
    params: BoardParams,
}

impl BoardCorrector {
    pub fn new(params: BoardParams) -> Result<Self> {
        if let Some(c) = &params.corners {
            senca_image::frame::check_dims(params.output_width, params.output_height)?;
            homography::rectify_homography(c, params.output_width, params.output_height)
                .ok_or(ImageError::Mismatch("4 隅が退化している"))?;
        }
        Ok(Self { params })
    }

    pub fn params(&self) -> &BoardParams {
        &self.params
    }

    /// 1 フレームを補正する。
    pub fn process(&self, f: &I420Frame) -> Result<I420Frame> {
        let p = &self.params;
        let warped;
        let src = match &p.corners {
            Some(c) => {
                warped = warp_i420(
                    f,
                    c,
                    p.output_width,
                    p.output_height,
                    p.enhance.spec.black(),
                )?;
                &warped
            }
            None => f,
        };
        if p.enhance_enabled {
            enhance_i420(src, &p.enhance)
        } else {
            Ok(src.clone())
        }
    }
}
