//! AudioSpecificConfig（ISO/IEC 14496-3 1.6.2.1）の解析と組み立て。

use crate::error::{Error, Result};
use senca_core::bits::{BitReader, BitWriter};

/// samplingFrequencyIndex の表（0〜12）。
pub const SAMPLING_FREQUENCIES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// 標本化周波数から samplingFrequencyIndex を引く（表にない値は `None`）。
pub fn sampling_frequency_index(rate: u32) -> Option<u8> {
    SAMPLING_FREQUENCIES
        .iter()
        .position(|&r| r == rate)
        .map(|i| i as u8)
}

/// channelConfiguration からチャンネル数を求める（0 は PCE、予約値は `None`）。
pub fn channels_of_configuration(cfg: u8) -> Option<u8> {
    match cfg {
        1..=6 => Some(cfg),
        7 => Some(8),
        11 => Some(7),
        12 | 14 => Some(8),
        13 => Some(24),
        _ => None,
    }
}

/// GASpecificConfig を持つ audioObjectType。
fn is_general_audio(aot: u8) -> bool {
    matches!(aot, 1 | 2 | 3 | 4 | 6 | 7 | 17 | 19 | 20 | 21 | 22 | 23)
}

/// AudioSpecificConfig の解析結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSpecificConfig {
    /// 最初に書かれた audioObjectType（codecs 文字列の N。明示的な HE-AAC なら 5、HE-AAC v2 なら 29）。
    pub signaled_object_type: u8,
    /// コアの audioObjectType（HE-AAC でも 2 = LC）。
    pub object_type: u8,
    /// samplingFrequencyIndex（周波数を直接書いた場合は `None`）。
    pub sampling_frequency_index: Option<u8>,
    /// コアの標本化周波数。
    pub sampling_frequency: u32,
    pub channel_configuration: u8,
    /// チャンネル数（channelConfiguration が 0 なら PCE から数えたもの）。PS による 2ch 化は含めない。
    pub channels: u8,
    /// 1 なら 960 サンプル/フレーム、0 なら 1024。
    pub frame_length_flag: bool,
    pub sbr_present: bool,
    pub ps_present: bool,
    /// SBR の出力周波数。
    pub extension_sampling_frequency: Option<u32>,
}

struct R<'a>(BitReader<'a>);

impl R<'_> {
    fn u(&mut self, n: u32) -> Result<u32> {
        self.0
            .read_bits(n)
            .map(|v| v as u32)
            .ok_or(Error::Truncated)
    }
    fn flag(&mut self) -> Result<bool> {
        self.0.read_bit().ok_or(Error::Truncated)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.0.skip(n).ok_or(Error::Truncated)
    }
    fn aot(&mut self) -> Result<u8> {
        let t = self.u(5)? as u8;
        let t = if t == 31 { 32 + self.u(6)? as u8 } else { t };
        if t == 0 {
            return Err(Error::Invalid("audioObjectType が 0"));
        }
        Ok(t)
    }
    fn freq(&mut self) -> Result<(Option<u8>, u32)> {
        let idx = self.u(4)? as u8;
        if idx == 15 {
            let f = self.u(24)?;
            if f == 0 {
                return Err(Error::Invalid("samplingFrequency が 0"));
            }
            return Ok((None, f));
        }
        let f = *SAMPLING_FREQUENCIES
            .get(idx as usize)
            .ok_or(Error::Invalid("samplingFrequencyIndex が予約値"))?;
        Ok((Some(idx), f))
    }
}

/// program_config_element を読み、チャンネル数を返す（4.4.1.1）。
fn parse_pce(r: &mut R<'_>) -> Result<u8> {
    r.skip(4 + 2 + 4)?; // element_instance_tag, object_type, sampling_frequency_index
    let front = r.u(4)?;
    let side = r.u(4)?;
    let back = r.u(4)?;
    let lfe = r.u(2)?;
    let assoc = r.u(3)?;
    let cc = r.u(4)?;
    if r.flag()? {
        r.skip(4)?;
    }
    if r.flag()? {
        r.skip(4)?;
    }
    if r.flag()? {
        r.skip(3)?;
    }
    let mut channels = 0u32;
    for _ in 0..front + side + back {
        channels += if r.flag()? { 2 } else { 1 };
        r.skip(4)?;
    }
    channels += lfe;
    r.skip(4 * lfe as usize + 4 * assoc as usize + 5 * cc as usize)?;
    // byte_alignment（AudioSpecificConfig の先頭から数える）
    let pad = (8 - r.0.position() % 8) % 8;
    r.skip(pad)?;
    let comment = r.u(8)? as usize;
    r.skip(comment * 8)?;
    if channels == 0 {
        return Err(Error::Invalid("PCE のチャンネル数が 0"));
    }
    Ok(channels as u8)
}

/// 後方互換の SBR/PS シグナリングを探す。見つかれば（SBR の周波数, PS の有無）。
fn probe_sync_extension(br: BitReader<'_>) -> Option<(u32, bool)> {
    let mut p = R(br);
    if p.0.bits_left() < 16 || p.u(11).ok()? != 0x2b7 || p.aot().ok()? != 5 || !p.flag().ok()? {
        return None;
    }
    let ext = p.freq().ok()?.1;
    let ps = p.0.bits_left() >= 12 && p.u(11).ok()? == 0x548 && p.flag().ok()?;
    Some((ext, ps))
}

fn write_aot(w: &mut BitWriter, aot: u8) {
    if aot >= 32 {
        w.write_bits(31, 5);
        w.write_bits((aot - 32) as u64, 6);
    } else {
        w.write_bits(aot as u64, 5);
    }
}

fn write_freq(w: &mut BitWriter, rate: u32) -> Result<()> {
    match sampling_frequency_index(rate) {
        Some(i) => w.write_bits(i as u64, 4),
        None => {
            if rate == 0 || rate >= 1 << 24 {
                return Err(Error::Invalid("標本化周波数"));
            }
            w.write_bits(15, 4);
            w.write_bits(rate as u64, 24);
        }
    }
    Ok(())
}

impl AudioSpecificConfig {
    /// LC などの単純な設定を作る（channels は 1〜6、8）。
    pub fn new(object_type: u8, sample_rate: u32, channels: u8) -> Result<Self> {
        let channel_configuration = match channels {
            1..=6 => channels,
            8 => 7,
            _ => return Err(Error::Unsupported("PCE が要るチャンネル数")),
        };
        if object_type == 0 || object_type > 95 {
            return Err(Error::Invalid("audioObjectType"));
        }
        if sample_rate == 0 || sample_rate >= 1 << 24 {
            return Err(Error::Invalid("標本化周波数"));
        }
        Ok(AudioSpecificConfig {
            signaled_object_type: object_type,
            object_type,
            sampling_frequency_index: sampling_frequency_index(sample_rate),
            sampling_frequency: sample_rate,
            channel_configuration,
            channels,
            frame_length_flag: false,
            sbr_present: false,
            ps_present: false,
            extension_sampling_frequency: None,
        })
    }

    /// バイト列を解析する。末尾の余りは無視する。
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut r = R(BitReader::new(data));
        let signaled = r.aot()?;
        let (sampling_frequency_index, sampling_frequency) = r.freq()?;
        let channel_configuration = r.u(4)? as u8;
        let mut object_type = signaled;
        let mut sbr_present = false;
        let mut ps_present = false;
        let mut extension_sampling_frequency = None;
        if signaled == 5 || signaled == 29 {
            sbr_present = true;
            ps_present = signaled == 29;
            extension_sampling_frequency = Some(r.freq()?.1);
            object_type = r.aot()?;
            if object_type == 22 {
                r.skip(4)?; // extensionChannelConfiguration
            }
        }
        let mut frame_length_flag = false;
        let mut channels = channels_of_configuration(channel_configuration);
        if is_general_audio(object_type) {
            frame_length_flag = r.flag()?;
            if r.flag()? {
                r.skip(14)?; // coreCoderDelay
            }
            let extension_flag = r.flag()?;
            if channel_configuration == 0 {
                channels = Some(parse_pce(&mut r)?);
            }
            if object_type == 6 || object_type == 20 {
                r.skip(3)?; // layerNr
            }
            if extension_flag {
                if object_type == 22 {
                    r.skip(5 + 11)?;
                }
                if matches!(object_type, 17 | 19 | 20 | 23) {
                    r.skip(3)?;
                }
                r.skip(1)?; // extensionFlag3
            }
            // 後方互換の SBR/PS シグナリング（syncExtensionType 0x2b7）。ER 系は epConfig があるので見ない。
            // 読めなければ（後ろが別のデータなら）無いものとして扱う。
            if !sbr_present
                && object_type < 17
                && let Some((ext, ps)) = probe_sync_extension(r.0.clone())
            {
                sbr_present = true;
                ps_present = ps;
                extension_sampling_frequency = Some(ext);
            }
        }
        let channels = match channels {
            Some(c) => c,
            None if channel_configuration == 0 => {
                return Err(Error::Unsupported("PCE で表した非 GA のチャンネル構成"));
            }
            None => return Err(Error::Invalid("channelConfiguration が予約値")),
        };
        Ok(AudioSpecificConfig {
            signaled_object_type: signaled,
            object_type,
            sampling_frequency_index,
            sampling_frequency,
            channel_configuration,
            channels,
            frame_length_flag,
            sbr_present,
            ps_present,
            extension_sampling_frequency,
        })
    }

    /// バイト列にする。GA（LC など）で channelConfiguration が 0 でないものだけ。
    ///
    /// SBR/PS は `signaled_object_type` が 5/29 なら明示的な階層シグナリング、
    /// そうでなければ後方互換シグナリング（syncExtensionType）で書く。
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        if !is_general_audio(self.object_type) {
            return Err(Error::Unsupported("GA 以外の audioObjectType の組み立て"));
        }
        if self.channel_configuration == 0 || self.channel_configuration > 15 {
            return Err(Error::Unsupported("PCE の組み立て"));
        }
        let mut w = BitWriter::new();
        let hierarchical = matches!(self.signaled_object_type, 5 | 29);
        write_aot(&mut w, self.signaled_object_type);
        write_freq(&mut w, self.sampling_frequency)?;
        w.write_bits(self.channel_configuration as u64, 4);
        if hierarchical {
            let ext = self
                .extension_sampling_frequency
                .ok_or(Error::Invalid("SBR の周波数がない"))?;
            write_freq(&mut w, ext)?;
            write_aot(&mut w, self.object_type);
        }
        w.write_bit(self.frame_length_flag);
        w.write_bit(false); // dependsOnCoreCoder
        w.write_bit(false); // extensionFlag
        if matches!(self.object_type, 6 | 20) {
            w.write_bits(0, 3);
        }
        if !hierarchical && self.sbr_present {
            let ext = self
                .extension_sampling_frequency
                .ok_or(Error::Invalid("SBR の周波数がない"))?;
            w.write_bits(0x2b7, 11);
            write_aot(&mut w, 5);
            w.write_bit(true);
            write_freq(&mut w, ext)?;
            if self.ps_present {
                w.write_bits(0x548, 11);
                w.write_bit(true);
            }
        }
        Ok(w.finish())
    }

    /// RFC 6381 の codecs 文字列（`mp4a.40.N`）。
    pub fn codecs_string(&self) -> String {
        format!("mp4a.40.{}", self.signaled_object_type)
    }

    /// 1 フレームのサンプル数（コアの周波数で 1024 または 960）。
    pub fn samples_per_frame(&self) -> u32 {
        if self.frame_length_flag { 960 } else { 1024 }
    }

    /// 出力の標本化周波数（SBR があれば SBR の周波数）。
    pub fn output_sampling_frequency(&self) -> u32 {
        match (self.sbr_present, self.extension_sampling_frequency) {
            (true, Some(f)) => f,
            _ => self.sampling_frequency,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lc_common() {
        // 48 kHz ステレオ LC = 0x11 0x90
        let a = AudioSpecificConfig::parse(&[0x11, 0x90]).unwrap();
        assert_eq!(a.object_type, 2);
        assert_eq!(a.sampling_frequency, 48000);
        assert_eq!(a.channels, 2);
        assert_eq!(a.codecs_string(), "mp4a.40.2");
        assert_eq!(a.to_bytes().unwrap(), vec![0x11, 0x90]);
        assert_eq!(AudioSpecificConfig::new(2, 48000, 2).unwrap(), a);
        assert_eq!(a.samples_per_frame(), 1024);
        // 44.1 kHz モノラル = 0x12 0x08
        let a = AudioSpecificConfig::parse(&[0x12, 0x08]).unwrap();
        assert_eq!((a.sampling_frequency, a.channels), (44100, 1));
    }

    #[test]
    fn he_aac_hierarchical_and_backward_compatible() {
        // HE-AAC v1: AOT 5、24 kHz、2ch、ext 48 kHz、core AOT 2
        let mut a = AudioSpecificConfig::new(2, 24000, 2).unwrap();
        a.signaled_object_type = 5;
        a.sbr_present = true;
        a.extension_sampling_frequency = Some(48000);
        let b = a.to_bytes().unwrap();
        assert_eq!(b, vec![0x2B, 0x11, 0x88, 0x00]);
        assert_eq!(AudioSpecificConfig::parse(&b).unwrap(), a);
        assert_eq!(a.codecs_string(), "mp4a.40.5");
        assert_eq!(a.output_sampling_frequency(), 48000);
        // HE-AAC v2
        a.signaled_object_type = 29;
        a.ps_present = true;
        let b = a.to_bytes().unwrap();
        assert_eq!(AudioSpecificConfig::parse(&b).unwrap(), a);
        assert_eq!(a.codecs_string(), "mp4a.40.29");
        // 後方互換シグナリング
        a.signaled_object_type = 2;
        let b = a.to_bytes().unwrap();
        let p = AudioSpecificConfig::parse(&b).unwrap();
        assert_eq!(p, a);
        assert!(p.sbr_present && p.ps_present);
        assert_eq!(p.codecs_string(), "mp4a.40.2");
    }

    #[test]
    fn escape_and_explicit_frequency() {
        // AOT 42（USAC 相当、エスケープ）、周波数 32000 を直接記述、2ch
        let mut w = BitWriter::new();
        write_aot(&mut w, 42);
        w.write_bits(15, 4);
        w.write_bits(32001, 24);
        w.write_bits(2, 4);
        let b = w.finish();
        let a = AudioSpecificConfig::parse(&b).unwrap();
        assert_eq!(a.object_type, 42);
        assert_eq!(a.sampling_frequency_index, None);
        assert_eq!(a.sampling_frequency, 32001);
        assert_eq!(a.codecs_string(), "mp4a.40.42");
        assert!(a.to_bytes().is_err());
        // LC で表にない周波数
        let a = AudioSpecificConfig::new(2, 32001, 1).unwrap();
        let b = a.to_bytes().unwrap();
        assert_eq!(AudioSpecificConfig::parse(&b).unwrap(), a);
    }

    #[test]
    fn pce_channel_count() {
        // LC 48 kHz、channelConfiguration 0、PCE: front 2（CPE＋SCE）、back 1（CPE）、LFE 1 → 6ch
        let mut w = BitWriter::new();
        write_aot(&mut w, 2);
        w.write_bits(3, 4);
        w.write_bits(0, 4);
        w.write_bits(0, 3); // GASpecificConfig
        w.write_bits(0, 4);
        w.write_bits(1, 2);
        w.write_bits(3, 4);
        w.write_bits(2, 4); // front
        w.write_bits(0, 4); // side
        w.write_bits(1, 4); // back
        w.write_bits(1, 2); // lfe
        w.write_bits(0, 3);
        w.write_bits(0, 4);
        w.write_bits(0, 3); // mixdown flags
        w.write_bits(0b0_0000, 5); // front SCE
        w.write_bits(0b1_0000, 5); // front CPE
        w.write_bits(0b1_0001, 5); // back CPE
        w.write_bits(0, 4); // lfe tag
        while !w.is_byte_aligned() {
            w.write_bit(false);
        }
        w.write_bits(2, 8);
        w.write_bits(0x4142, 16);
        let b = w.finish();
        let a = AudioSpecificConfig::parse(&b).unwrap();
        assert_eq!(a.channel_configuration, 0);
        assert_eq!(a.channels, 6);
        assert!(a.to_bytes().is_err());
        // コメントの途中で切れたもの
        assert_eq!(
            AudioSpecificConfig::parse(&b[..b.len() - 1]),
            Err(Error::Truncated)
        );
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(AudioSpecificConfig::parse(&[]), Err(Error::Truncated));
        assert_eq!(AudioSpecificConfig::parse(&[0x11]), Err(Error::Truncated));
        assert!(AudioSpecificConfig::parse(&[0x00, 0x10]).is_err()); // AOT 0
        assert!(AudioSpecificConfig::parse(&[0x16, 0x90]).is_err()); // index 13
        assert!(AudioSpecificConfig::parse(&[0x11, 0xC0]).is_err()); // channelConfiguration 8
        assert!(AudioSpecificConfig::parse(&[0x17, 0x80, 0, 0, 0x10]).is_err()); // 周波数 0
        assert!(AudioSpecificConfig::new(2, 48000, 0).is_err());
        assert!(AudioSpecificConfig::new(2, 48000, 7).is_err());
        assert!(AudioSpecificConfig::new(2, 0, 2).is_err());
        assert!(AudioSpecificConfig::new(0, 48000, 2).is_err());
        // すべての 2 バイト入力でパニックしない
        for a in 0..=255u8 {
            for b in 0..=255u8 {
                let _ = AudioSpecificConfig::parse(&[a, b]);
                let _ = AudioSpecificConfig::parse(&[a, b, 0xFF]);
            }
        }
    }

    #[test]
    fn channel_table() {
        assert_eq!(channels_of_configuration(7), Some(8));
        assert_eq!(channels_of_configuration(13), Some(24));
        assert_eq!(channels_of_configuration(0), None);
        assert_eq!(channels_of_configuration(9), None);
        let a = AudioSpecificConfig::new(2, 48000, 8).unwrap();
        assert_eq!(a.channel_configuration, 7);
    }
}
