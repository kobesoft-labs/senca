//! `CodecProvider`・`Encoder`・`Decoder`（仕様 11 章）。
//!
//! # 契約
//!
//! - **所有権。** 入力の AU・フレームは借用で受け、必要ならプロバイダーが内部で複製する（C のライブラリに渡すため）。
//!   出力は `Bytes` を持つ値として返す。呼び出し側にライブラリ内部のポインターは見せない（仕様 11 章）。
//! - **スレッド。** `CodecProvider` は `Send + Sync`（検出結果を共有する）。`Encoder`・`Decoder` は `Send` だけで、
//!   1 つのスレッドから順に呼ぶ。コールバックで呼び出し側に戻ることはない（再入しない）。
//! - **取り消し・時間切れ。** 呼び出しは同期でブロックする。時間切れと取り消しは、これを動かす codec worker の
//!   プロセスごと止めることで実現する（`senca-codec-worker` の `WorkerPool`）。
//! - **送る・受け取る。** `send` で 1 つ入れ、`receive` が `Ok(None)` を返すまで取り出す。出力が溜まっている間は
//!   `send` が [`ErrorKind::Internal`](crate::ErrorKind) を返すことはない（内部で溜める）が、呼び出し側は
//!   `send` ごとに `receive` で取り出すこと。`flush` の後は `receive` で残りをすべて取り出し、以後 `send` は受け付けない。
//! - **範囲。** コーデック処理以外（URL 接続、mux/demux、任意のフィルター）は提供しない（仕様 11 章）。

use crate::caps::{Capability, DeviceInfo, Direction, ProviderId, ProviderStatus};
use crate::config::{DecoderConfig, EncoderConfig};
use crate::error::Result;
use crate::frame::Frame;
use senca_core::{AccessUnit, Codec, CodecConfig};

/// コーデックの提供者（ハードウェア SDK またはソフトウェアのライブラリ）。
pub trait CodecProvider: Send + Sync {
    fn id(&self) -> ProviderId;

    /// 検出の結果。使えないときも理由を返す（落ちない）。
    fn status(&self) -> ProviderStatus;

    /// 検出した装置。使えないときは空。
    fn devices(&self) -> Vec<DeviceInfo>;

    /// 能力の一覧。使えないときは空。
    fn capabilities(&self) -> Vec<Capability>;

    /// エンコーダーを作る。対応しないときは `Unsupported`、装置・枠が無いときは `Unavailable`。
    fn create_encoder(&self, config: &EncoderConfig) -> Result<Box<dyn Encoder>>;

    /// デコーダーを作る。
    fn create_decoder(&self, config: &DecoderConfig) -> Result<Box<dyn Decoder>>;

    /// 指定のコーデック・方向に対応するか（能力から求める）。
    fn supports(&self, codec: Codec, direction: Direction) -> bool {
        self.capabilities()
            .iter()
            .any(|c| c.codec == codec && c.direction == direction)
    }
}

/// エンコーダー（フレームを入れて AU を取り出す）。
pub trait Encoder: Send {
    fn codec(&self) -> Codec;

    /// 出力トラックの設定（avcC・hvcC・av1C・OpusHead など、ADR-0003）。
    /// 最初の AU を出す前に分からないコーデックでは、最初の `receive` までは `None` のことがある。
    /// 設定が変わると `generation` が増え、以後の AU の `config_generation` がそれを指す。
    fn codec_config(&self) -> Option<CodecConfig>;

    /// フレームを 1 つ入れる。`pts` は設定の時間軸。
    fn send(&mut self, frame: &Frame) -> Result<()>;

    /// 符号化済みの AU を 1 つ取り出す（内部形式、ADR-0003）。無ければ `None`。
    fn receive(&mut self) -> Result<Option<AccessUnit>>;

    /// 入力の終わり。以後は `receive` で残りを取り出す。
    fn flush(&mut self) -> Result<()>;

    /// 次に入れるフレームをキーフレームにする。
    fn request_keyframe(&mut self);
}

/// デコーダー（AU を入れてフレームを取り出す）。
pub trait Decoder: Send {
    fn codec(&self) -> Codec;

    /// トラックの設定が変わったとき（`CodecConfig.generation` が増えたとき）に呼ぶ。
    fn set_config(&mut self, config: &CodecConfig) -> Result<()>;

    /// AU を 1 つ入れる（内部形式、ADR-0003）。
    fn send(&mut self, au: &AccessUnit) -> Result<()>;

    /// 復号したフレームを 1 つ取り出す。無ければ `None`。`pts` は入力 AU の時間軸。
    fn receive(&mut self) -> Result<Option<Frame>>;

    /// 入力の終わり。以後は `receive` で残りを取り出す。
    fn flush(&mut self) -> Result<()>;
}
