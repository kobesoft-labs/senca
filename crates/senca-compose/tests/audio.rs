//! 音声ミキサーの試験（正弦波・Goertzel）。

use senca_compose::{AudioMixer, InputConfig, LimiterConfig, MixerConfig, OUTPUT_RATE, Resampler};

fn sine(rate: u32, ch: usize, freq: f64, amp: f32, n: usize, phase0: usize) -> Vec<f32> {
    let mut v = Vec::with_capacity(n * ch);
    for i in 0..n {
        let s = amp
            * (2.0 * std::f64::consts::PI * freq * (i + phase0) as f64 / rate as f64).sin() as f32;
        for _ in 0..ch {
            v.push(s);
        }
    }
    v
}

/// Goertzel で求めた振幅。
fn goertzel_amp(x: &[f32], rate: u32, freq: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * freq / rate as f64;
    let c = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &v in x {
        let s = v as f64 + c * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    let p = s1 * s1 + s2 * s2 - c * s1 * s2;
    2.0 * p.max(0.0).sqrt() / x.len() as f64
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

fn channel(x: &[f32], c: usize) -> Vec<f32> {
    x.iter().skip(c).step_by(2).copied().collect()
}

/// 入力を 10 ms ずつ押し込み、48 kHz で `secs` 秒混ぜる。
fn run(
    m: &mut AudioMixer,
    id: u32,
    rate: u32,
    ch: usize,
    freq: f64,
    amp: f32,
    secs: f64,
) -> Vec<f32> {
    let chunk = (rate / 100) as usize;
    let total = (rate as f64 * secs) as usize;
    let mut out = Vec::new();
    let mut i = 0;
    while i < total {
        let x = sine(rate, ch, freq, amp, chunk, i);
        m.push(id, (i as i64 * 1_000_000) / rate as i64, &x)
            .unwrap();
        i += chunk;
        // 出力は入力より 20 ms 遅らせて取り出す（リサンプラーの待ちを吸収）
        let t_out = (i as i64 * 1_000_000) / rate as i64 - 20_000;
        if t_out >= 0 {
            let start = out.len() as i64 / 2;
            let want = senca_compose::mixer::us_to_pos(t_out) - start;
            if want > 0 {
                out.extend(m.mix_at(start, want as usize));
            }
        }
    }
    out
}

#[test]
fn resample_44100_preserves_frequency_and_level() {
    for (freq, tol_db) in [(1000.0, 0.02), (15000.0, 0.05), (18000.0, 0.1)] {
        let mut m = AudioMixer::new(MixerConfig {
            limiter: LimiterConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        });
        m.add_input(1, 44100, 1, InputConfig::default()).unwrap();
        let out = run(&mut m, 1, 44100, 1, freq, 0.5, 1.5);
        let l = channel(&out, 0);
        let seg = &l[4800..4800 + 48000];
        let a = goertzel_amp(seg, OUTPUT_RATE, freq);
        let db = 20.0 * (a / 0.5).log10();
        // 残り（歪み＋雑音）
        let total = rms(seg) * std::f64::consts::SQRT_2;
        let resid = (total.powi(2) - a.powi(2)).max(0.0).sqrt();
        let snr = 20.0 * (a / resid.max(1e-12)).log10();
        println!("{freq} Hz: 利得 {db:+.4} dB、SNR {snr:.1} dB");
        assert!(db.abs() < tol_db, "{freq} Hz: {db} dB");
        assert!(snr > 70.0, "{freq} Hz: SNR {snr}");
    }
}

#[test]
fn downsample_rejects_aliases() {
    // 96 kHz の 30 kHz は 48 kHz では 18 kHz に折り返す。消えていること。
    let mut r = Resampler::new(96000, 48000, 1);
    let x = sine(96000, 1, 30000.0, 0.5, 96000, 0);
    let mut y = Vec::new();
    r.process(&x, &mut y);
    let seg = &y[2000..2000 + 40000];
    let lvl = 20.0 * (rms(seg) / (0.5 / std::f64::consts::SQRT_2)).log10();
    println!("30 kHz → 折り返しの残り {lvl:.1} dB");
    assert!(lvl < -70.0, "{lvl}");
    // 通過域（10 kHz）はそのまま
    let mut r = Resampler::new(96000, 48000, 1);
    let x = sine(96000, 1, 10000.0, 0.5, 96000, 0);
    let mut y = Vec::new();
    r.process(&x, &mut y);
    let a = goertzel_amp(&y[2000..2000 + 43200], 48000, 10000.0);
    assert!((20.0 * (a / 0.5).log10()).abs() < 0.02);
}

#[test]
fn gain_pan_mute_solo() {
    let cfg = MixerConfig {
        limiter: LimiterConfig {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut m = AudioMixer::new(cfg);
    m.add_input(
        1,
        48000,
        2,
        InputConfig {
            gain_db: -6.0206,
            ..Default::default()
        },
    )
    .unwrap();
    m.add_input(
        2,
        48000,
        1,
        InputConfig {
            pan: -1.0,
            ..Default::default()
        },
    )
    .unwrap();
    let x1 = sine(48000, 2, 1000.0, 0.4, 4800, 0);
    let x2 = sine(48000, 1, 3000.0, 0.4, 4800, 0);
    m.push(1, 0, &x1).unwrap();
    m.push(2, 0, &x2).unwrap();
    let out = m.mix(0, 4800);
    let (l, r) = (channel(&out, 0), channel(&out, 1));
    assert!((goertzel_amp(&r, 48000, 1000.0) - 0.2).abs() < 1e-3);
    assert!(
        goertzel_amp(&r, 48000, 3000.0) < 1e-4,
        "左に振ったので右には出ない"
    );
    assert!((goertzel_amp(&l, 48000, 3000.0) - 0.4).abs() < 1e-3);
    let meters = m.meters();
    let m1 = meters.inputs.iter().find(|(id, _)| *id == 1).unwrap().1;
    assert!((m1.peak_db[0] - (-13.98)).abs() < 0.1, "{:?}", m1); // 0.2 = −13.98 dBFS
    assert!((m1.rms_db[0] - (-16.99)).abs() < 0.1, "{:?}", m1);
    // ソロ: 2 だけ
    m.set_input_config(
        2,
        InputConfig {
            pan: -1.0,
            solo: true,
            ..Default::default()
        },
    )
    .unwrap();
    m.push(1, 100_000, &x1).unwrap();
    m.push(2, 100_000, &x2).unwrap();
    let out = m.mix(100_000, 4800);
    assert!(goertzel_amp(&channel(&out, 1), 48000, 1000.0) < 1e-4);
    // ミュート
    m.set_input_config(
        2,
        InputConfig {
            mute: true,
            solo: true,
            ..Default::default()
        },
    )
    .unwrap();
    m.push(2, 200_000, &x2).unwrap();
    let out = m.mix(200_000, 4800);
    assert!(out.iter().all(|&v| v == 0.0));
    assert!(m.set_input_config(9, InputConfig::default()).is_err());
    assert!(
        m.set_input_config(
            1,
            InputConfig {
                pan: 2.0,
                ..Default::default()
            }
        )
        .is_err()
    );
}

#[test]
fn limiter_prevents_clipping() {
    let mut m = AudioMixer::new(MixerConfig::default());
    for id in 0..3 {
        m.add_input(id, 48000, 2, InputConfig::default()).unwrap();
    }
    let mut out = Vec::new();
    for k in 0..50 {
        let x = sine(48000, 2, 440.0, 0.9, 960, k * 960);
        for id in 0..3 {
            m.push(id, k as i64 * 20_000, &x).unwrap();
        }
        out.extend(m.mix(k as i64 * 20_000, 960));
    }
    let ceiling = 10f32.powf(-1.0 / 20.0);
    let peak = out.iter().fold(0f32, |a, &v| a.max(v.abs()));
    println!(
        "リミッター: ピーク {peak:.4}（天井 {ceiling:.4}）、下げ幅 {:.1} dB",
        m.meters().limiter_reduction_db
    );
    assert!(peak <= ceiling + 1e-6);
    assert!(peak > ceiling * 0.95, "下げすぎ {peak}");
    assert!(m.meters().limiter_reduction_db < -6.0);
    // 先読みの遅延は 1.5 ms 程度
    assert!(m.latency_frames() <= 72);
}

#[test]
fn missing_input_and_gaps_become_silence() {
    let cfg = MixerConfig {
        limiter: LimiterConfig {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut m = AudioMixer::new(cfg);
    m.add_input(1, 48000, 1, InputConfig::default()).unwrap();
    // 0〜100 ms を入れ、100 ms 空けて 200〜300 ms
    m.push(1, 0, &sine(48000, 1, 1000.0, 0.5, 4800, 0)).unwrap();
    m.push(1, 200_000, &sine(48000, 1, 1000.0, 0.5, 4800, 9600))
        .unwrap();
    let out = m.mix(0, 19200);
    let l = channel(&out, 0);
    assert!(rms(&l[0..4800]) > 0.3);
    assert!(rms(&l[4800..9600]) == 0.0, "隙間は無音");
    assert!(rms(&l[9600..14400]) > 0.3, "200 ms の位置に置かれる");
    assert!(rms(&l[14400..]) == 0.0, "データの無い所は無音");
    let st = m.input_stats(1).unwrap();
    assert_eq!(st.gap_frames, 4800);
    assert_eq!(st.underrun_frames, 4800);
    assert_eq!(st.resyncs, 1);
    assert!(m.meters().inputs[0].1.peak_db[0] > -7.0);
    let out = m.mix(400_000, 480);
    assert!(out.iter().all(|&v| v == 0.0));
    assert_eq!(m.meters().inputs[0].1.peak_db[0], -120.0);
    // 壊れた入力
    assert!(m.push(1, 0, &[f32::NAN; 7]).is_ok());
    assert!(m.push(7, 0, &[0.0; 2]).is_err());
}

#[test]
fn jitter_is_absorbed_and_delay_shifts() {
    let cfg = MixerConfig {
        limiter: LimiterConfig {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut m = AudioMixer::new(cfg);
    m.add_input(
        1,
        44100,
        2,
        InputConfig {
            delay_ms: 10.0,
            ..Default::default()
        },
    )
    .unwrap();
    let mut out = Vec::new();
    let mut s = 1u32;
    for k in 0..100 {
        let x = sine(44100, 2, 1000.0, 0.5, 441, k * 441);
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        let jitter = (s >> 16) as i64 % 6000 - 3000; // ±3 ms
        m.push(1, k as i64 * 10_000 + jitter, &x).unwrap();
        if k >= 3 {
            out.extend(m.mix((k as i64 - 3) * 10_000, 480));
        }
    }
    let st = m.input_stats(1).unwrap();
    assert_eq!(st.resyncs, 0, "揺らぎで再同期しない");
    assert_eq!(st.gap_frames, 0);
    let l = channel(&out, 0);
    // 遅延 10 ms: 最初の 480 標本は無音、その後に音
    assert!(l[..470].iter().all(|&v| v == 0.0));
    assert!(rms(&l[600..1200]) > 0.3);
    let seg = &l[4800..4800 + 38400];
    let a = goertzel_amp(seg, 48000, 1000.0);
    let resid = ((rms(seg) * std::f64::consts::SQRT_2).powi(2) - a * a)
        .max(0.0)
        .sqrt();
    assert!(20.0 * (a / resid).log10() > 60.0, "途切れがない");
}

#[test]
fn surround_downmix() {
    let cfg = MixerConfig {
        limiter: LimiterConfig {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut m = AudioMixer::new(cfg);
    m.add_input(1, 48000, 6, InputConfig::default()).unwrap();
    // C だけに音
    let mut x = vec![0f32; 480 * 6];
    for i in 0..480 {
        x[i * 6 + 2] = 0.5;
    }
    m.push(1, 0, &x).unwrap();
    let out = m.mix(0, 480);
    assert!((out[100] - 0.5 * std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    assert!((out[101] - 0.5 * std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    assert!(m.push(1, 0, &[0.0; 5]).is_err());
    assert!(m.add_input(2, 1000, 2, InputConfig::default()).is_err());
    assert!(m.add_input(1, 48000, 2, InputConfig::default()).is_err());
}
