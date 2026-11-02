//! 合成・ミキサーの誤り。

use std::fmt;

use senca_image::ImageError;

#[derive(Debug, Clone, PartialEq)]
pub enum ComposeError {
    /// 画像処理の誤り。
    Image(ImageError),
    /// 設定の値が不正。
    InvalidConfig(&'static str),
    /// 知らない入力（音声）。
    UnknownInput(u32),
    /// 同じ番号の入力がすでにある。
    DuplicateInput(u32),
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Image(e) => write!(f, "画像処理: {e}"),
            Self::InvalidConfig(m) => write!(f, "設定の誤り: {m}"),
            Self::UnknownInput(i) => write!(f, "知らない入力 {i}"),
            Self::DuplicateInput(i) => write!(f, "入力 {i} はすでにある"),
        }
    }
}

impl std::error::Error for ComposeError {}

impl From<ImageError> for ComposeError {
    fn from(e: ImageError) -> Self {
        Self::Image(e)
    }
}

pub type Result<T> = std::result::Result<T, ComposeError>;
