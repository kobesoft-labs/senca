//! 共通の型（役割・種類・コーデック・部屋の設定）。シグナリングと REST の両方で同じ JSON の形にする。

use serde::{Deserialize, Serialize};
use senca_core::media::{Codec, MediaKind};

use crate::filter::ForwardingFilter;

/// 参加の役割（role）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Sendrecv,
    Sendonly,
    Recvonly,
}

impl Role {
    pub fn can_publish(self) -> bool {
        matches!(self, Role::Sendrecv | Role::Sendonly)
    }

    pub fn can_subscribe(self) -> bool {
        matches!(self, Role::Sendrecv | Role::Recvonly)
    }

    /// `self` が `max` の許す範囲か（sendrecv はすべてを許す）。
    pub fn within(self, max: Role) -> bool {
        max == Role::Sendrecv || self == max
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Sendrecv => "sendrecv",
            Role::Sendonly => "sendonly",
            Role::Recvonly => "recvonly",
        }
    }
}

/// トラックの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Audio,
    Video,
}

impl Kind {
    pub fn from_media(k: MediaKind) -> Option<Kind> {
        match k {
            MediaKind::Audio => Some(Kind::Audio),
            MediaKind::Video => Some(Kind::Video),
            _ => None,
        }
    }

    pub fn media(self) -> MediaKind {
        match self {
            Kind::Audio => MediaKind::Audio,
            Kind::Video => MediaKind::Video,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Audio => "audio",
            Kind::Video => "video",
        }
    }
}

/// 映像のコーデック（部屋で 1 つ。SFU は変換しないので、送り手と受け手で同じでなければならない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    #[default]
    Vp8,
    Vp9,
    H264,
    Av1,
}

impl VideoCodec {
    pub fn codec(self) -> Codec {
        match self {
            VideoCodec::Vp8 => Codec::Vp8,
            VideoCodec::Vp9 => Codec::Vp9,
            VideoCodec::H264 => Codec::H264,
            VideoCodec::Av1 => Codec::Av1,
        }
    }
}

/// 音声のコーデック。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AudioCodec {
    #[default]
    Opus,
}

impl AudioCodec {
    pub fn codec(self) -> Codec {
        Codec::Opus
    }
}

/// スポットライトで注目されていない送り手の映像の扱い。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Unfocused {
    /// 一番低い層を配る。
    #[default]
    Lowest,
    /// 配らない（枠は残し、`paused` にする）。
    Off,
}

/// スポットライトの設定（部屋）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpotlightSettings {
    /// 注目する話者の数（1〜16）。
    pub number: u8,
    #[serde(default)]
    pub unfocused: Unfocused,
    /// 注目した送り手の層（`None` なら帯域で自動）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_rid: Option<String>,
}

/// 録画の方針（実際の録画はメディアパイプライン、ADR-0016）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecordingMode {
    #[default]
    None,
    /// 参加者ごと（E2EE でも暗号のまま保存できる）。
    PerParticipant,
    /// 合成（E2EE の部屋ではできない）。
    Composite,
}

/// 部屋の設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoomSettings {
    /// 参加者の上限（ノードをまたぐ参加者も数える）。
    #[serde(default = "default_max_participants")]
    pub max_participants: u32,
    /// E2EE の部屋（SFU は中身を見ずに配る。合成・合成の録画はできない）。
    #[serde(default)]
    pub e2ee: bool,
    #[serde(default)]
    pub video_codec: VideoCodec,
    #[serde(default)]
    pub audio_codec: AudioCodec,
    /// 送り手に勧める映像の上限（kbit/s、`connected` で伝える）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_video_bitrate_kbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spotlight: Option<SpotlightSettings>,
    #[serde(default)]
    pub recording: RecordingMode,
    /// 部屋全体の転送フィルター（購読者ごとのフィルターと両方を満たすものだけ配る）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarding_filter: Option<ForwardingFilter>,
    /// 使ってよい DataChannel のラベルの型（`#chat`、末尾の `*` は前方一致）。
    #[serde(default = "default_labels")]
    pub data_labels: Vec<String>,
    /// 誰もいなくなってから消すまでの秒数（参加で自動に作った部屋だけ）。
    #[serde(default = "default_empty_timeout")]
    pub empty_timeout_secs: u32,
    /// カスケード: 部屋を置いたノード（home）以外のノードにつないだ参加者を、そのノード（edge）で受ける
    /// （ノード間の中継で home とつなぐ）。偽なら部屋を置いたノードへ案内（redirect）する。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cascade: bool,
    /// この部屋の認証 Webhook（http/https）。ノードの設定より優先する。署名の秘密はノードの設定のもの。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_webhook_url: Option<String>,
}

fn default_max_participants() -> u32 {
    50
}

fn default_labels() -> Vec<String> {
    vec!["#*".into()]
}

fn default_empty_timeout() -> u32 {
    60
}

impl Default for RoomSettings {
    fn default() -> Self {
        RoomSettings {
            max_participants: default_max_participants(),
            e2ee: false,
            video_codec: VideoCodec::Vp8,
            audio_codec: AudioCodec::Opus,
            max_video_bitrate_kbps: None,
            spotlight: None,
            recording: RecordingMode::None,
            forwarding_filter: None,
            data_labels: default_labels(),
            empty_timeout_secs: default_empty_timeout(),
            cascade: false,
            auth_webhook_url: None,
        }
    }
}

/// 設定の誤り（どのフィールドか）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldError {
    pub field: String,
    pub reason: String,
}

impl FieldError {
    pub fn new(field: &str, reason: impl Into<String>) -> FieldError {
        FieldError {
            field: field.into(),
            reason: reason.into(),
        }
    }
}

/// rid（RFC 8851）として使える形か（英数字 1〜16 文字）。
pub fn valid_rid(r: &str) -> bool {
    !r.is_empty() && r.len() <= 16 && r.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// 部屋の ID（URL に入れる）。
pub fn valid_room_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'@'))
}

/// Webhook の URL の形（宛先の検査はノードの egress の方針で、送る時に行う）。
pub fn valid_webhook_url(u: &str) -> bool {
    let Some(rest) = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://")) else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    u.len() <= 2048 && !host.is_empty() && !host.contains('@') && u.bytes().all(|b| b.is_ascii_graphic())
}

/// DataChannel のラベルの型に一致するか（`#*` は `#` で始まるすべて）。
pub fn label_matches(pattern: &str, label: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => label.starts_with(prefix),
        None => pattern == label,
    }
}

impl RoomSettings {
    /// 設定を検査する。
    pub fn validate(&self) -> Result<(), Vec<FieldError>> {
        let mut e = Vec::new();
        if !(1..=10_000).contains(&self.max_participants) {
            e.push(FieldError::new("max_participants", "1〜10000"));
        }
        if let Some(s) = &self.spotlight {
            if !(1..=16).contains(&s.number) {
                e.push(FieldError::new("spotlight.number", "1〜16"));
            }
            if s.focus_rid.as_deref().is_some_and(|r| !valid_rid(r)) {
                e.push(FieldError::new("spotlight.focus_rid", "英数字 1〜16 文字"));
            }
        }
        if self.e2ee && self.recording == RecordingMode::Composite {
            e.push(FieldError::new(
                "recording",
                "E2EE の部屋はサーバーで中身を見られないので合成の録画はできない",
            ));
        }
        if let Some(f) = &self.forwarding_filter
            && let Err(fe) = f.validate()
        {
            e.extend(fe.into_iter().map(|mut x| {
                x.field = format!("forwarding_filter.{}", x.field);
                x
            }));
        }
        if self.data_labels.len() > 32
            || self
                .data_labels
                .iter()
                .any(|l| !l.starts_with('#') || l.len() > 64)
        {
            e.push(FieldError::new("data_labels", "`#` で始まる 64 byte までの型を 32 個まで"));
        }
        if self.max_video_bitrate_kbps.is_some_and(|b| !(50..=50_000).contains(&b)) {
            e.push(FieldError::new("max_video_bitrate_kbps", "50〜50000"));
        }
        if self.auth_webhook_url.as_deref().is_some_and(|u| !valid_webhook_url(u)) {
            e.push(FieldError::new(
                "auth_webhook_url",
                "http:// か https:// で始まる 2048 byte までの URL（利用者情報・空白なし）",
            ));
        }
        if e.is_empty() { Ok(()) } else { Err(e) }
    }

    /// サーバーで合成（映像の合成・合成の録画・文字起こしのための復号）を計画できるか。
    /// できなければ理由（計画に返す）。
    pub fn compositing_allowed(&self) -> Result<(), &'static str> {
        if self.e2ee {
            Err("e2ee: the SFU forwards end-to-end encrypted media without access to its content, so server-side compositing is impossible")
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles() {
        assert!(Role::Sendonly.within(Role::Sendrecv));
        assert!(Role::Recvonly.within(Role::Recvonly));
        assert!(!Role::Sendrecv.within(Role::Recvonly));
        assert!(!Role::Sendonly.within(Role::Recvonly));
        assert!(Role::Sendonly.can_publish() && !Role::Sendonly.can_subscribe());
        assert_eq!(serde_json::to_string(&Role::Recvonly).unwrap(), "\"recvonly\"");
    }

    #[test]
    fn settings_defaults_and_validation() {
        let s: RoomSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, RoomSettings::default());
        assert!(s.validate().is_ok());
        assert!(serde_json::from_str::<RoomSettings>(r#"{"bogus":1}"#).is_err());
        let bad: RoomSettings = serde_json::from_str(
            r#"{"e2ee":true,"recording":"composite","max_participants":0,"spotlight":{"number":0},"data_labels":["chat"]}"#,
        )
        .unwrap();
        let fields: Vec<String> = bad.validate().unwrap_err().into_iter().map(|e| e.field).collect();
        assert_eq!(fields, vec!["max_participants", "spotlight.number", "recording", "data_labels"]);
        // カスケード・認証 Webhook（既定は出さない: 前と同じ JSON）
        assert_eq!(serde_json::to_value(RoomSettings::default()).unwrap().get("cascade"), None);
        let c: RoomSettings =
            serde_json::from_str(r#"{"cascade":true,"auth_webhook_url":"https://auth.example/hook"}"#).unwrap();
        assert!(c.cascade && c.validate().is_ok());
        for u in ["ftp://x/", "https://", "https://u:p@x/", "https://x/ a", "javascript:alert(1)"] {
            let s = RoomSettings {
                auth_webhook_url: Some(u.into()),
                ..Default::default()
            };
            assert_eq!(s.validate().unwrap_err()[0].field, "auth_webhook_url", "{u}");
        }
        let e2ee = RoomSettings {
            e2ee: true,
            ..Default::default()
        };
        assert!(e2ee.compositing_allowed().unwrap_err().starts_with("e2ee"));
        assert!(RoomSettings::default().compositing_allowed().is_ok());
    }

    #[test]
    fn labels_and_ids() {
        assert!(label_matches("#*", "#chat"));
        assert!(label_matches("#chat", "#chat"));
        assert!(!label_matches("#chat", "#chat2"));
        assert!(label_matches("#game-*", "#game-1"));
        assert!(!label_matches("#*", "signaling"));
        assert!(valid_room_id("room-1_a.b"));
        assert!(!valid_room_id("a/b"));
        assert!(!valid_room_id(""));
        assert!(valid_rid("r0") && !valid_rid("r-0") && !valid_rid(""));
    }
}
