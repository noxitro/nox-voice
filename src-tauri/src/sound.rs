//! 通知音 — 録音開始とキャンセル/エラーを**耳で**分かるようにする。
//!
//! # なぜ合成音なのか
//!
//! ペダル運用 (Stream Deck ペダル等) では、ユーザーは画面を見ていない。
//! 「踏んだのに録音が始まっていなかった」に気づくのが発話し終わった後だと、
//! 話した内容がまるごと失われる。オーバーレイは目で見る合図なので、
//! **画面を見ない運用では合図にならない**。
//!
//! 音源はコードで生成する。WAV を同梱するとライセンスの管理が要るうえ、
//! バイナリが太る。合成なら「プリセットを増やす = 関数を 1 つ足す」で済み、
//! どのサンプリングレートにも合わせて作れる。
//!
//! # 再生方式
//!
//! `PlaySoundW` に `SND_MEMORY` でメモリ上の WAV をそのまま渡す。
//! cpal の出力ストリームを組むより初期化が軽く、既定の出力デバイスの
//! 選択・切り替えを OS に任せられる。
//!
//! `SND_SYNC` を**専用スレッド**で呼ぶ。`SND_ASYNC` だと呼び出しから
//! 戻った後も OS がバッファを読み続けるため、`Vec` を落とすと
//! 解放済みメモリを読ませることになる。同期再生をスレッドへ追い出せば、
//! バッファはそのスレッドが持ったまま再生の終わりまで生き、
//! 呼び出し側 (録音開始経路) は 1 ミリ秒も待たない。

use std::io::Cursor;
use std::path::Path;
use std::thread;

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_MEMORY, SND_NODEFAULT, SND_SYNC};

/// 合成音のサンプリングレート。通知音には 44.1 kHz で十分すぎる。
const SYNTH_SAMPLE_RATE: u32 = 44_100;

/// カスタム音として読み込む WAV の上限 (秒)。
///
/// 通知音に長い曲を指定されると、次の録音開始まで鳴りっぱなしになる。
/// 超過分は切り詰める (拒否はしない — 鳴らないより頭出しでも鳴る方がよい)。
const MAX_CUSTOM_SECONDS: f32 = 5.0;

/// 音量の既定値 (%)。
pub const DEFAULT_VOLUME: u8 = 60;

/// プリセット音。`Custom` はユーザー指定の WAV ファイル。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SoundPreset {
    /// 鳴らさない。イベントごとに個別に黙らせられる。
    Silent,
    /// やわらかいポップ音。既定 (短く、耳に刺さらない)。
    #[default]
    SoftPop,
    /// 2 音の上昇チャイム。
    Chime,
    /// マリンバ風の木の音。
    Marimba,
    /// ベル (倍音が非整数で、余韻が長い)。
    Bell,
    /// 8bit 風のブリップ。
    Blip,
    /// 上昇スイープ。「始まった」感が強い。
    Rise,
    /// 下降スイープ。取り消し向き。
    Fall,
    /// 澄んだディン。
    Ding,
    /// 低いノック。周囲に響きにくい。
    Knock,
    /// ソナー風の正弦波 + 長い余韻。
    Sonar,
    /// ノイズのウーッシュ。
    Whoosh,
    /// 低い 2 連ブザー。エラー向き。
    Buzz,
    /// ユーザー指定の WAV ファイル。
    Custom,
}

impl SoundPreset {
    /// UI に出す日本語名。
    pub fn label(self) -> &'static str {
        match self {
            SoundPreset::Silent => "鳴らさない",
            SoundPreset::SoftPop => "ソフトポップ",
            SoundPreset::Chime => "チャイム",
            SoundPreset::Marimba => "マリンバ",
            SoundPreset::Bell => "ベル",
            SoundPreset::Blip => "ブリップ (8bit)",
            SoundPreset::Rise => "上昇スイープ",
            SoundPreset::Fall => "下降スイープ",
            SoundPreset::Ding => "ディン",
            SoundPreset::Knock => "ノック",
            SoundPreset::Sonar => "ソナー",
            SoundPreset::Whoosh => "ウーッシュ",
            SoundPreset::Buzz => "ブザー",
            SoundPreset::Custom => "ファイルを指定",
        }
    }

    /// 設定 UI に並べる順。`Custom` を最後に置く。
    pub fn all() -> &'static [SoundPreset] {
        &[
            SoundPreset::SoftPop,
            SoundPreset::Chime,
            SoundPreset::Marimba,
            SoundPreset::Bell,
            SoundPreset::Blip,
            SoundPreset::Rise,
            SoundPreset::Fall,
            SoundPreset::Ding,
            SoundPreset::Knock,
            SoundPreset::Sonar,
            SoundPreset::Whoosh,
            SoundPreset::Buzz,
            SoundPreset::Silent,
            SoundPreset::Custom,
        ]
    }

    /// serde と同じ識別子 (フロントとの受け渡しに使う)。
    pub fn id(self) -> &'static str {
        match self {
            SoundPreset::Silent => "silent",
            SoundPreset::SoftPop => "soft_pop",
            SoundPreset::Chime => "chime",
            SoundPreset::Marimba => "marimba",
            SoundPreset::Bell => "bell",
            SoundPreset::Blip => "blip",
            SoundPreset::Rise => "rise",
            SoundPreset::Fall => "fall",
            SoundPreset::Ding => "ding",
            SoundPreset::Knock => "knock",
            SoundPreset::Sonar => "sonar",
            SoundPreset::Whoosh => "whoosh",
            SoundPreset::Buzz => "buzz",
            SoundPreset::Custom => "custom",
        }
    }
}

/// 鳴らす音の指定 (プリセット + カスタム時のファイルパス)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundChoice {
    pub preset: SoundPreset,
    /// `preset == Custom` のときだけ使う WAV のパス。
    pub custom_path: String,
}

impl SoundChoice {
    pub fn new(preset: SoundPreset, custom_path: &str) -> Self {
        Self {
            preset,
            custom_path: custom_path.to_string(),
        }
    }
}

/// 音を鳴らす。**呼び出し側は待たない** (再生は別スレッド)。
///
/// 失敗はログに残すだけで、呼び出し側の処理には一切影響させない。
/// 通知音が鳴らないことを理由に録音を止めるのは本末転倒。
pub fn play(choice: &SoundChoice, volume: u8) {
    if choice.preset == SoundPreset::Silent || volume == 0 {
        return;
    }
    let wav = match render_wav(choice, volume) {
        Ok(wav) => wav,
        Err(e) => {
            log::warn!("通知音を生成できません: {e}");
            return;
        }
    };
    play_rendered(wav);
}

/// 生成済みの WAV を鳴らす (試聴用)。呼び出し側は待たない。
pub fn play_rendered(wav: Vec<u8>) {
    let spawned = thread::Builder::new()
        .name("nox-sound".to_string())
        .spawn(move || play_wav_blocking(&wav));
    if let Err(e) = spawned {
        log::warn!("通知音の再生スレッドを起動できません: {e}");
    }
}

/// 指定された音を WAV バイト列にする (テストから検証できるよう分離)。
pub fn render_wav(choice: &SoundChoice, volume: u8) -> Result<Vec<u8>, String> {
    let (samples, rate) = match choice.preset {
        SoundPreset::Silent => return Err("鳴らさない設定です".to_string()),
        SoundPreset::Custom => load_custom(Path::new(choice.custom_path.trim()))?,
        preset => (synthesize(preset, SYNTH_SAMPLE_RATE), SYNTH_SAMPLE_RATE),
    };
    if samples.is_empty() {
        return Err("音声データが空です".to_string());
    }
    Ok(encode_wav(&samples, rate, gain(volume)))
}

/// 音量 (%) を振幅倍率へ。
///
/// 人の耳は音圧に対して対数的なので、% をそのまま振幅にすると
/// 50% がほとんど「少し小さい」程度にしか感じられない。
/// 2 乗を噛ませて、つまみの半分がちゃんと半分くらいに聞こえるようにする。
fn gain(volume: u8) -> f32 {
    let v = f32::from(volume.min(100)) / 100.0;
    v * v
}

/// メモリ上の WAV を鳴らし切るまでブロックする。
fn play_wav_blocking(wav: &[u8]) {
    // SAFETY: SND_MEMORY では第 1 引数を WAV イメージへのポインタとして扱う。
    // `wav` はこの関数が返るまで生きており、SND_SYNC なので
    // PlaySoundW から戻った時点で OS はもうこのバッファを読まない。
    // SND_NODEFAULT: 鳴らせないときに Windows の既定音で代用させない
    // (通知音として意味が変わってしまう)。
    let ok = unsafe {
        PlaySoundW(
            PCWSTR(wav.as_ptr().cast::<u16>()),
            None,
            SND_MEMORY | SND_SYNC | SND_NODEFAULT,
        )
    };
    if !ok.as_bool() {
        log::warn!("通知音を再生できませんでした (PlaySoundW が失敗)");
    }
}

/// f32 サンプル列を 16bit モノラル WAV へ。音量はここで掛ける。
fn encode_wav(samples: &[f32], sample_rate: u32, gain: f32) -> Vec<u8> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = Cursor::new(Vec::<u8>::new());
    {
        // Cursor 相手の書き込みなので IO エラーは起きない。
        let mut writer = hound::WavWriter::new(&mut cursor, spec).expect("WAV ヘッダを書ける");
        for s in samples {
            let v = (s * gain).clamp(-1.0, 1.0);
            let _ = writer.write_sample((v * f32::from(i16::MAX)) as i16);
        }
        let _ = writer.finalize();
    }
    cursor.into_inner()
}

/// ユーザー指定の WAV を読む。長すぎるものは頭から [`MAX_CUSTOM_SECONDS`] 秒で切る。
///
/// 多チャンネルはモノラルへ畳む (通知音に定位は要らない)。
fn load_custom(path: &Path) -> Result<(Vec<f32>, u32), String> {
    if path.as_os_str().is_empty() {
        return Err("音声ファイルが指定されていません".to_string());
    }
    let mut reader = hound::WavReader::open(path)
        .map_err(|e| format!("音声ファイルを読めません ({}): {e}", path.display()))?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels.max(1));
    let limit = (spec.sample_rate as f32 * MAX_CUSTOM_SECONDS) as usize * channels;

    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .take(limit)
            .filter_map(Result::ok)
            .collect(),
        hound::SampleFormat::Int => {
            // 16 / 24 / 32 bit をまとめて i32 で受け、ビット深度で正規化する。
            let scale = 1.0 / (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .take(limit)
                .filter_map(Result::ok)
                .map(|v| v as f32 * scale)
                .collect()
        }
    };
    if raw.is_empty() {
        return Err(format!("音声ファイルが空です: {}", path.display()));
    }
    let mono = if channels == 1 {
        raw
    } else {
        raw.chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
            .collect()
    };
    Ok((mono, spec.sample_rate))
}

// --- 合成 -------------------------------------------------------------------

/// プリセットの波形を作る。振幅は ±1.0 に収める (音量は encode で掛ける)。
fn synthesize(preset: SoundPreset, rate: u32) -> Vec<f32> {
    match preset {
        // 800 Hz の正弦に速い減衰。クリックにならない程度のアタック。
        SoundPreset::SoftPop => tone(rate, 0.09, |t| sine(t, 800.0) * decay(t, 28.0)),
        // C6 → E6 の 2 音。「開始」を明るく伝える。
        SoundPreset::Chime => sequence(rate, &[(0.07, 1046.5), (0.14, 1318.5)], 20.0),
        // 基音 + 4 倍音の木質感。マリンバは奇数倍音が薄い。
        SoundPreset::Marimba => tone(rate, 0.22, |t| {
            (sine(t, 587.3) + 0.35 * sine(t, 587.3 * 4.0)) * decay(t, 22.0) * 0.75
        }),
        // 非整数倍音を重ねると金属的になる。余韻を長めに。
        SoundPreset::Bell => tone(rate, 0.55, |t| {
            (sine(t, 880.0) * decay(t, 6.0)
                + 0.5 * sine(t, 880.0 * 2.76) * decay(t, 10.0)
                + 0.25 * sine(t, 880.0 * 5.4) * decay(t, 16.0))
                // 3 つの部分音が立ち上がりで揃うので、合計は単純な和より
                // 大きくなる。0.55 まで下げて ±1.0 に収める。
                * 0.55
        }),
        // 矩形波 + 短い減衰。レトロゲームの決定音。
        SoundPreset::Blip => tone(rate, 0.08, |t| square(t, 987.8) * decay(t, 40.0) * 0.5),
        // 440 → 1320 Hz の上昇。踏み込みの合図に向く。
        SoundPreset::Rise => sweep(rate, 0.16, 440.0, 1320.0),
        // 1320 → 440 Hz の下降。取り消しの合図に向く。
        SoundPreset::Fall => sweep(rate, 0.16, 1320.0, 440.0),
        // 高めの正弦 2 倍音。事務的で邪魔にならない。
        SoundPreset::Ding => tone(rate, 0.3, |t| {
            (sine(t, 1568.0) + 0.3 * sine(t, 3136.0)) * decay(t, 14.0) * 0.7
        }),
        // 低い減衰音。夜間や共有スペース向き。
        SoundPreset::Knock => tone(rate, 0.12, |t| {
            (sine(t, 180.0) + 0.4 * sine(t, 320.0)) * decay(t, 45.0) * 0.7
        }),
        // 細い正弦に長い余韻。押しっぱなし運用でも耳が疲れにくい。
        SoundPreset::Sonar => tone(rate, 0.5, |t| sine(t, 660.0) * decay(t, 7.0) * 0.8),
        // ノイズを短い山型の包絡で。声とかぶらない帯域感。
        SoundPreset::Whoosh => {
            let mut noise = Noise::new(0x5EED_1234);
            tone(rate, 0.18, move |t| {
                let env = (t * 40.0).min(1.0) * decay(t, 18.0);
                noise.next() * env * 0.35
            })
        }
        // 低い矩形波を 2 回。エラーは「気持ちよくない音」であるべき。
        SoundPreset::Buzz => {
            let gap = 0.09f32;
            tone(rate, 0.22, move |t| {
                let second = (gap..gap + 0.07).contains(&t);
                if t < 0.07 {
                    square(t, 220.0) * decay(t, 25.0) * 0.4
                } else if second {
                    let phase = t - gap;
                    square(phase, 220.0) * decay(phase, 25.0) * 0.4
                } else {
                    0.0
                }
            })
        }
        // 到達しない (呼び出し側で分岐済み) が、無音を返して安全側に倒す。
        SoundPreset::Silent | SoundPreset::Custom => Vec::new(),
    }
}

/// `duration` 秒ぶんのサンプルを `f(t)` から作る。
///
/// 末尾 5 ms をフェードアウトさせる。途中で切ると段差が「プチッ」と鳴る。
fn tone(rate: u32, duration: f32, mut f: impl FnMut(f32) -> f32) -> Vec<f32> {
    let total = (rate as f32 * duration) as usize;
    let fade = (rate as f32 * 0.005) as usize;
    (0..total)
        .map(|i| {
            let t = i as f32 / rate as f32;
            let tail = if total > fade && i + fade > total {
                (total - i) as f32 / fade as f32
            } else {
                1.0
            };
            f(t) * tail
        })
        .collect()
}

/// 音を順に鳴らす (それぞれ独立に減衰させる)。
fn sequence(rate: u32, notes: &[(f32, f32)], decay_rate: f32) -> Vec<f32> {
    let mut out = Vec::new();
    for (duration, freq) in notes {
        out.extend(tone(rate, *duration, |t| {
            sine(t, *freq) * decay(t, decay_rate) * 0.8
        }));
    }
    out
}

/// 周波数を直線的に動かすスイープ。
///
/// 位相は周波数の積分で進める。毎サンプル `sin(2πft)` に周波数を
/// 入れ直すと位相が飛び、ザラついた音になる。
fn sweep(rate: u32, duration: f32, from: f32, to: f32) -> Vec<f32> {
    let mut phase = 0.0f32;
    let step = 1.0 / rate as f32;
    tone(rate, duration, move |t| {
        let progress = (t / duration).clamp(0.0, 1.0);
        let freq = from + (to - from) * progress;
        phase += std::f32::consts::TAU * freq * step;
        // 出だしと終わりを丸める (両端の段差を作らない)。
        let env = (t * 60.0).min(1.0) * decay(t, 6.0);
        phase.sin() * env * 0.8
    })
}

fn sine(t: f32, freq: f32) -> f32 {
    (std::f32::consts::TAU * freq * t).sin()
}

fn square(t: f32, freq: f32) -> f32 {
    if (freq * t).fract() < 0.5 {
        1.0
    } else {
        -1.0
    }
}

/// 指数減衰の包絡。`rate` が大きいほど短い。
fn decay(t: f32, rate: f32) -> f32 {
    (-t * rate).exp()
}

/// 決定的な擬似乱数 (xorshift)。
///
/// `rand` を足さないのは、通知音のために依存を増やす価値が無いため。
/// 種を固定するので、同じプリセットは毎回同じ音になる。
struct Noise(u32);

impl Noise {
    fn new(seed: u32) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_renders_audible_samples() {
        for preset in SoundPreset::all() {
            if matches!(preset, SoundPreset::Silent | SoundPreset::Custom) {
                continue;
            }
            let samples = synthesize(*preset, SYNTH_SAMPLE_RATE);
            assert!(!samples.is_empty(), "{preset:?} が空");
            let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            assert!(peak > 0.05, "{preset:?} のピークが小さすぎる: {peak}");
            assert!(peak <= 1.0, "{preset:?} のピークが 1.0 を超える: {peak}");
        }
    }

    /// 末尾が 0 付近で終わっていないと、再生の切れ目で「プチッ」と鳴る。
    #[test]
    fn presets_fade_out_at_the_end() {
        for preset in SoundPreset::all() {
            if matches!(preset, SoundPreset::Silent | SoundPreset::Custom) {
                continue;
            }
            let samples = synthesize(*preset, SYNTH_SAMPLE_RATE);
            let last = samples.last().copied().unwrap_or(0.0).abs();
            assert!(last < 0.05, "{preset:?} の末尾が段差になっている: {last}");
        }
    }

    #[test]
    fn volume_scales_the_waveform() {
        assert_eq!(gain(0), 0.0);
        assert!((gain(100) - 1.0).abs() < f32::EPSILON);
        assert!(gain(50) < 0.5, "対数的に効くこと");
    }

    #[test]
    fn rendered_wav_has_riff_header() {
        let choice = SoundChoice::new(SoundPreset::SoftPop, "");
        let wav = render_wav(&choice, 100).expect("生成できる");
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
    }

    #[test]
    fn silent_and_missing_custom_are_errors_not_panics() {
        assert!(render_wav(&SoundChoice::new(SoundPreset::Silent, ""), 100).is_err());
        assert!(render_wav(&SoundChoice::new(SoundPreset::Custom, ""), 100).is_err());
        assert!(render_wav(&SoundChoice::new(SoundPreset::Custom, "Z:\\nope.wav"), 100).is_err());
    }

    #[test]
    fn preset_ids_match_serde_representation() {
        for preset in SoundPreset::all() {
            let json = serde_json::to_string(preset).expect("直列化");
            assert_eq!(json, format!("\"{}\"", preset.id()));
        }
    }

    /// 多チャンネル WAV はモノラルへ畳んで読めること。
    #[test]
    fn custom_wav_roundtrip() {
        let dir = std::env::temp_dir().join("nox-sound-test");
        std::fs::create_dir_all(&dir).expect("一時ディレクトリ");
        let path = dir.join("stereo.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 22_050,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("書き出し");
        for i in 0..1000 {
            let v = ((i as f32 / 10.0).sin() * 8000.0) as i16;
            writer.write_sample(v).expect("L");
            writer.write_sample(v).expect("R");
        }
        writer.finalize().expect("finalize");

        let (mono, rate) = load_custom(&path).expect("読める");
        assert_eq!(rate, 22_050);
        assert_eq!(mono.len(), 1000);
        let _ = std::fs::remove_file(&path);
    }
}
