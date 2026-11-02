//! 画像処理の誤り。

use std::fmt;

/// 画像処理・符号化・復号の誤り。壊れた入力や上限超えはすべてここに落ちる（パニックしない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// 幅・高さが 0、または組み合わせが不正。
    InvalidDimensions { width: usize, height: usize },
    /// 上限（`Limits`）を超えた。
    TooLarge { width: usize, height: usize },
    /// 引数の不一致（大きさの違う画像を比べた、など）。
    Mismatch(&'static str),
    /// 形式が壊れている。
    Format(&'static str),
    /// 形式としては正しいが対応していない（プログレッシブ JPEG など）。
    Unsupported(&'static str),
    /// データが途中で切れている。
    Truncated,
    /// 検査値（CRC32・Adler32）が合わない。
    Checksum(&'static str),
    /// 展開後の大きさが上限を超えた（zip bomb 対策）。
    OutputLimit,
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDimensions { width, height } => {
                write!(f, "不正な大きさ {width}x{height}")
            }
            Self::TooLarge { width, height } => write!(f, "大きすぎる画像 {width}x{height}"),
            Self::Mismatch(m) => write!(f, "引数の不一致: {m}"),
            Self::Format(m) => write!(f, "形式の誤り: {m}"),
            Self::Unsupported(m) => write!(f, "未対応: {m}"),
            Self::Truncated => write!(f, "データが途中で切れている"),
            Self::Checksum(m) => write!(f, "検査値の不一致: {m}"),
            Self::OutputLimit => write!(f, "展開後の大きさが上限を超えた"),
        }
    }
}

impl std::error::Error for ImageError {}

/// この crate の Result。
pub type Result<T> = std::result::Result<T, ImageError>;
