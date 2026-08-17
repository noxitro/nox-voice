//! 音声録音 — cpal でデフォルト入力デバイスから取り、16 kHz / mono / 16bit の
//! WAV バイト列をメモリ上に生成する。
//!
//! 出力形式は Whisper (Groq `whisper-large-v3`) の入力に合わせてある。
//! デバイスが 16 kHz mono を直接サポートしていればそのまま使い、
//! そうでなければネイティブ形式で取ってから [`resample_mono`] で変換する。

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::Sender;
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig, SupportedStreamConfig,
};

/// STT へ渡す WAV のサンプリングレート。
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// 録音の長さ上限 (10 分)。到達したら自動停止させる。
///
/// 単なるバッファ上限にして黙って切り捨てると、ユーザーには
/// 「後半が消えた録音」としてしか見えない。到達を [`start`] に渡した
/// チャネルで通知し、呼び出し側が停止と可視化を行う。
pub const MAX_RECORDING_SECONDS: f64 = 600.0;

/// 録音まわりのエラー。すべてユーザーに可視化できる日本語メッセージを持つ。
#[derive(Debug)]
pub enum AudioError {
    /// 入力デバイスが 1 台も無い。
    NoInputDevice,
    /// デバイス設定の取得に失敗。
    ConfigUnavailable(String),
    /// このサンプル形式には対応していない。
    UnsupportedFormat(SampleFormat),
    /// ストリームの生成・開始に失敗。
    StreamFailed(String),
    /// 録音データが空 (押下が短すぎた等)。
    EmptyRecording,
    /// WAV エンコードに失敗。
    Encode(String),
    /// 内部状態の破損 (ロックの毒化など)。
    Internal(String),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::NoInputDevice => {
                write!(f, "録音デバイスが見つかりません。マイクの接続と Windows のマイクアクセス許可を確認してください")
            }
            AudioError::ConfigUnavailable(e) => {
                write!(f, "録音デバイスの設定を取得できません: {e}")
            }
            AudioError::UnsupportedFormat(fmt) => {
                write!(f, "対応していないサンプル形式です: {fmt}")
            }
            AudioError::StreamFailed(e) => write!(f, "録音の開始に失敗しました: {e}"),
            AudioError::EmptyRecording => {
                write!(f, "録音データが空です (押下が短すぎた可能性があります)")
            }
            AudioError::Encode(e) => write!(f, "WAV の生成に失敗しました: {e}"),
            AudioError::Internal(e) => write!(f, "録音の内部エラー: {e}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// 進行中の録音。drop するとストリームは止まる。
///
/// cpal 0.18 の `Stream` は `Send + Sync` が保証されているため、
/// 専用スレッドを立てずに共有状態へ持てる。
pub struct Recorder {
    /// `Option` にしてあるのは [`finish`] で明示的に落とすため。
    stream: Option<Stream>,
    /// コールバックが mono f32 を追記する先。
    buffer: Arc<Mutex<Vec<f32>>>,
    source_sample_rate: u32,
    device_name: String,
    started_at: SystemTime,
    /// 長さ上限に到達したか。
    limit_hit: Arc<AtomicBool>,
    /// 入力レベル (オーバーレイのメーター用)。
    meter: Arc<LevelMeter>,
}

impl Recorder {
    /// 録音開始時刻。
    pub fn started_at(&self) -> SystemTime {
        self.started_at
    }

    /// 使用中のデバイス名 (ログ用)。
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// 長さ上限に到達して打ち切られたか。
    pub fn limit_reached(&self) -> bool {
        self.limit_hit.load(Ordering::SeqCst)
    }

    /// 入力レベルの共有スロット (録音中の可視化に使う)。
    pub fn meter(&self) -> Arc<LevelMeter> {
        Arc::clone(&self.meter)
    }

    /// テスト用: 実デバイスなしで [`finish`] にかけられる Recorder を作る。
    ///
    /// ストリームを持たないので `finish` はバッファをそのまま変換する。
    /// 終了時の退避経路など、Recorder を必要とする配線のテストに使う。
    #[cfg(test)]
    pub fn for_test(samples: Vec<f32>, source_sample_rate: u32) -> Self {
        Self {
            stream: None,
            buffer: Arc::new(Mutex::new(samples)),
            source_sample_rate,
            device_name: "<test device>".to_string(),
            started_at: SystemTime::now(),
            limit_hit: Arc::new(AtomicBool::new(false)),
            meter: Arc::new(LevelMeter::new()),
        }
    }
}

/// 入力レベル (RMS) の共有スロット。
///
/// 音声コールバックが毎回書き、UI 側のスレッドが読む。
/// **コールバックからイベントを送らない** — 1 秒に何十回も IPC を叩くと
/// WebView 側が詰まるし、音声コールバックの時間予算も食う。
/// ここへ置くだけにして、送るのは別スレッドが間引いて行う。
#[derive(Debug, Default)]
pub struct LevelMeter {
    /// 直近の RMS を f32 のビット表現で持つ (atomic に置ける形)。
    level_bits: AtomicU32,
}

impl LevelMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// コールバックから呼ぶ。モノラル化済みサンプルの RMS を記録する。
    pub fn record(&self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        let sum: f32 = samples.iter().map(|s| s * s).sum();
        let rms = (sum / samples.len() as f32).sqrt();
        self.level_bits.store(rms.to_bits(), Ordering::Relaxed);
    }

    /// 直近の RMS を読む。
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level_bits.load(Ordering::Relaxed))
    }

    /// UI のメーター用に 0.0..=1.0 へ写す。
    ///
    /// RMS をそのまま出すと、通常の発話 (0.02〜0.1 くらい) がほぼ振れない。
    /// 対数にして「静か〜大きい」を見た目の差にする。
    pub fn display_level(&self) -> f32 {
        normalize_level(self.level())
    }
}

/// RMS を 0.0..=1.0 の表示値へ。純関数なのでテストできる。
///
/// -60dB を下限、-6dB を上限として線形に伸ばす。
pub fn normalize_level(rms: f32) -> f32 {
    const MIN_DB: f32 = -60.0;
    const MAX_DB: f32 = -6.0;
    if !rms.is_finite() || rms <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * rms.log10();
    ((db - MIN_DB) / (MAX_DB - MIN_DB)).clamp(0.0, 1.0)
}

/// 音声コールバックが書き込む先一式。
///
/// コールバックはリアルタイム制約下にあるため、ここでの操作は
/// `try_lock` / `try_send` のみ (待たない・確保しない)。
#[derive(Clone)]
struct CaptureSink {
    buffer: Arc<Mutex<Vec<f32>>>,
    max_samples: usize,
    limit_hit: Arc<AtomicBool>,
    /// 上限到達を 1 度だけ通知する。容量 1 の有界チャネル。
    limit_tx: Sender<()>,
    /// 入力レベルの共有スロット。
    meter: Arc<LevelMeter>,
}

/// デフォルト入力デバイスで録音を開始する。
///
/// `limit_tx` には長さ上限 ([`MAX_RECORDING_SECONDS`]) に到達した時点で
/// 1 度だけ `()` が送られる。受け取った側は録音を停止し、ユーザーへ可視化すること。
pub fn start(limit_tx: Sender<()>) -> Result<Recorder, AudioError> {
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or(AudioError::NoInputDevice)?;
    let device_name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unknown device>".to_string());

    let config = pick_input_config(&device)?;
    let sample_format = config.sample_format();
    let channels = config.channels() as usize;
    let source_sample_rate = config.sample_rate();
    let stream_config: StreamConfig = config.config();

    log::info!(
        "録音開始: device=\"{device_name}\" {source_sample_rate}Hz {channels}ch {sample_format:?}"
    );

    let buffer = Arc::new(Mutex::new(Vec::<f32>::with_capacity(
        source_sample_rate as usize * 4,
    )));
    let limit_hit = Arc::new(AtomicBool::new(false));
    let meter = Arc::new(LevelMeter::new());
    let sink = CaptureSink {
        buffer: Arc::clone(&buffer),
        max_samples: (source_sample_rate as f64 * MAX_RECORDING_SECONDS) as usize,
        limit_hit: Arc::clone(&limit_hit),
        limit_tx,
        meter: Arc::clone(&meter),
    };

    let err_fn = |err: cpal::Error| {
        log::error!("録音ストリームのエラー: {err} ({:?})", err.kind());
    };

    // サンプル形式ごとに単相化する。中身はすべて `push_samples` に集約。
    macro_rules! build {
        ($t:ty) => {{
            let sink = sink.clone();
            device
                .build_input_stream::<$t, _, _>(
                    stream_config.clone(),
                    move |data: &[$t], _| push_samples(data, channels, &sink),
                    err_fn,
                    None,
                )
                .map_err(|e| AudioError::StreamFailed(e.to_string()))?
        }};
    }

    let stream = match sample_format {
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I32 => build!(i32),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        other => return Err(AudioError::UnsupportedFormat(other)),
    };

    stream
        .play()
        .map_err(|e| AudioError::StreamFailed(e.to_string()))?;

    Ok(Recorder {
        stream: Some(stream),
        buffer,
        source_sample_rate,
        device_name,
        started_at: SystemTime::now(),
        limit_hit,
        meter,
    })
}

/// 録音を停止し、16 kHz mono 16bit の WAV バイト列と実尺を返す。
pub fn finish(mut recorder: Recorder) -> Result<(Vec<u8>, Duration), AudioError> {
    // ストリームを落としてからバッファを取り出す (コールバックの追記を止める)。
    if let Some(stream) = recorder.stream.take() {
        if let Err(e) = stream.pause() {
            log::warn!("ストリームの停止に失敗 (drop で回収する): {e}");
        }
        drop(stream);
    }

    let samples = {
        let mut guard = recorder
            .buffer
            .lock()
            .map_err(|_| AudioError::Internal("録音バッファのロックが毒化した".to_string()))?;
        std::mem::take(&mut *guard)
    };

    if samples.is_empty() {
        return Err(AudioError::EmptyRecording);
    }

    let resampled = resample_mono(&samples, recorder.source_sample_rate, TARGET_SAMPLE_RATE);
    let duration = Duration::from_secs_f64(resampled.len() as f64 / TARGET_SAMPLE_RATE as f64);
    let wav = encode_wav(&resampled, TARGET_SAMPLE_RATE)?;
    Ok((wav, duration))
}

/// 16 kHz mono を直接サポートしていればそれを、無ければデバイス既定を選ぶ。
///
/// 直接 16 kHz を引ければリサンプルを丸ごと省ける (品質・CPU の両面で有利)。
fn pick_input_config(device: &cpal::Device) -> Result<SupportedStreamConfig, AudioError> {
    if let Ok(ranges) = device.supported_input_configs() {
        let native_16k = ranges
            .filter(|r| r.channels() == 1)
            .filter(|r| matches!(r.sample_format(), SampleFormat::F32 | SampleFormat::I16))
            .find_map(|r| r.try_with_sample_rate(TARGET_SAMPLE_RATE));
        if let Some(config) = native_16k {
            return Ok(config);
        }
    }
    device
        .default_input_config()
        .map_err(|e| AudioError::ConfigUnavailable(e.to_string()))
}

/// コールバック本体: インターリーブ入力をモノラル f32 に畳んで追記する。
///
/// 音声コールバックはリアルタイム制約下にあるので、ロックが取れなければ
/// そのチャンクは捨てる (待たない)。
fn push_samples<T>(data: &[T], channels: usize, sink: &CaptureSink)
where
    T: SizedSample,
    f32: FromSample<T>,
{
    if channels == 0 {
        return;
    }
    let Ok(mut buf) = sink.buffer.try_lock() else {
        return;
    };
    if buf.len() >= sink.max_samples {
        // 上限到達。初回だけ通知し、以降は静かに捨てる (停止は受信側の責務)。
        if !sink.limit_hit.swap(true, Ordering::SeqCst) {
            let _ = sink.limit_tx.try_send(());
        }
        return;
    }
    let before = buf.len();
    if channels == 1 {
        buf.extend(data.iter().map(|s| f32::from_sample(*s)));
    } else {
        let inv = 1.0 / channels as f32;
        for frame in data.chunks_exact(channels) {
            let sum: f32 = frame.iter().map(|s| f32::from_sample(*s)).sum();
            buf.push(sum * inv);
        }
    }
    // 今回書き足したぶんだけでレベルを測る (atomic への store だけ)。
    sink.meter.record(&buf[before..]);
}

/// f32 モノラルを 16bit PCM の WAV バイト列にする。
fn encode_wav(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>, AudioError> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    // ヘッダ 44 バイト + 本体。
    let mut cursor = Cursor::new(Vec::<u8>::with_capacity(44 + samples.len() * 2));
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec)
            .map_err(|e| AudioError::Encode(e.to_string()))?;
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            writer
                .write_sample(v)
                .map_err(|e| AudioError::Encode(e.to_string()))?;
        }
        writer
            .finalize()
            .map_err(|e| AudioError::Encode(e.to_string()))?;
    }
    Ok(cursor.into_inner())
}

/// テスト用: 任意のサンプル列から WAV を作る (stt の実 API 疎通テストで使う)。
#[cfg(test)]
pub fn encode_wav_for_test(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    encode_wav(samples, sample_rate).expect("テスト用 WAV の生成に失敗")
}

// --- リサンプラ (ポリフェーズ) -----------------------------------------------
//
// 用途が「録音終了後に一括で 1 本のモノラル信号を変換する」だけなので、
// ストリーミング用リサンプラ (rubato 等) は使わず窓関数付き sinc 補間を自前で書く。
// ダウンサンプル時の折り返し (エイリアシング) はカットオフを出力ナイキストに
// 合わせることでカーネルに内蔵される。
//
// # なぜポリフェーズか
//
// 素朴に「出力サンプルごとに sin/cos を都度評価する」実装は、
// 48kHz→16kHz (53 タップ) で 1 分の録音に release 0.6 秒・debug 14 秒/10 分 かかる。
// 変換比は有理数 L/M なので、出力サンプルが取りうる位相は L 種類しかない。
// 位相ごとにカーネルを 1 度だけ作って使い回せば、実行時は積和だけになる。

/// sinc カーネルの片側ローブ数。大きいほど遷移が急峻になるが計算量に比例する。
const SINC_LOBES: f64 = 8.0;
/// カットオフのマージン (ナイキストの 95%)。
const CUTOFF_MARGIN: f64 = 0.95;
/// 位相テーブルを作る上限。これを超える場合は都度計算にフォールバックする。
///
/// 位相数 L = dst / gcd(src, dst)。実在するデバイスレート
/// (8k/11.025k/16k/22.05k/32k/44.1k/48k/96k …) はいずれも 16000 と十分な
/// 公約数を持つので L は小さい (最大でも 44.1k の 160)。
/// 想定外のレートでテーブルが肥大化しないための保険。
const MAX_PHASES: usize = 4096;

/// 窓関数付き sinc カーネルの設計パラメータ。
struct SincDesign {
    /// 入力レート基準の正規化カットオフ (cycles/sample)。
    cutoff: f64,
    /// カーネルの片側幅 (入力サンプル数)。
    half_width: f64,
    half_width_i: isize,
}

impl SincDesign {
    fn new(ratio: f64) -> Self {
        let cutoff = 0.5 * ratio.min(1.0) * CUTOFF_MARGIN;
        let half_width = (SINC_LOBES / (2.0 * cutoff)).ceil();
        Self {
            cutoff,
            half_width,
            half_width_i: half_width as isize,
        }
    }

    fn taps(&self) -> usize {
        (self.half_width_i * 2 + 1) as usize
    }

    /// 位相 `frac` (0.0..1.0) のカーネルを `out` に書く。総和 1 に正規化する。
    ///
    /// タップ `k` は入力インデックス `base + (k - half_width_i)` に対応し、
    /// そこでの sinc の引数は `frac - (k - half_width_i)`。
    fn write_kernel(&self, frac: f64, out: &mut [f32]) {
        debug_assert_eq!(out.len(), self.taps());
        let mut norm = 0.0f64;
        for (k, slot) in out.iter_mut().enumerate() {
            let offset = k as isize - self.half_width_i;
            let x = frac - offset as f64;
            let w = blackman(x / self.half_width)
                * 2.0
                * self.cutoff
                * sinc(2.0 * self.cutoff * x);
            norm += w;
            *slot = w as f32;
        }
        if norm.abs() > 1e-12 {
            let inv = (1.0 / norm) as f32;
            for slot in out.iter_mut() {
                *slot *= inv;
            }
        }
    }
}

/// モノラル f32 を `src_rate` から `dst_rate` へ変換する。
///
/// ダウンサンプル時は出力側ナイキストにカットオフを合わせたローパスが
/// カーネルに内蔵されるため、別途フィルタは不要。
pub fn resample_mono(input: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if input.is_empty() || src_rate == 0 || dst_rate == 0 {
        return Vec::new();
    }
    if src_rate == dst_rate {
        return input.to_vec();
    }

    let ratio = dst_rate as f64 / src_rate as f64;
    let design = SincDesign::new(ratio);
    let taps = design.taps();
    let out_len = ((input.len() as f64) * ratio).round().max(1.0) as usize;
    let mut out = Vec::with_capacity(out_len);

    // 変換比を既約分数 L/M にする。出力 n の入力座標は n*M/L で、
    // 小数部 (= 位相) は n*M mod L の L 通りしかない。
    let g = gcd(src_rate, dst_rate);
    let phases = (dst_rate / g) as u64;
    let step = (src_rate / g) as u64;

    if phases as usize <= MAX_PHASES {
        // 位相ごとにカーネルを事前計算し、以降は積和のみ。
        let mut table = vec![0f32; phases as usize * taps];
        for p in 0..phases as usize {
            let frac = p as f64 / phases as f64;
            design.write_kernel(frac, &mut table[p * taps..(p + 1) * taps]);
        }
        for n in 0..out_len {
            let pos = n as u64 * step;
            let base = (pos / phases) as isize;
            let phase = (pos % phases) as usize;
            let kernel = &table[phase * taps..phase * taps + taps];
            out.push(convolve(input, base - design.half_width_i, kernel));
        }
    } else {
        // 位相数が過大なレート比 (実在デバイスではまず来ない)。
        // テーブルを諦めて都度カーネルを作る。結果は上と同一。
        let mut kernel = vec![0f32; taps];
        for n in 0..out_len {
            let pos = n as u64 * step;
            let base = (pos / phases) as isize;
            let frac = (pos % phases) as f64 / phases as f64;
            design.write_kernel(frac, &mut kernel);
            out.push(convolve(input, base - design.half_width_i, &kernel));
        }
    }
    out
}

/// `input[start..start+kernel.len()]` とカーネルの内積。
///
/// 範囲外は最近傍サンプルで延長する (ゼロ詰めだと端にフェード状の欠けが出る)。
fn convolve(input: &[f32], start: isize, kernel: &[f32]) -> f32 {
    let last = input.len() as isize - 1;
    // 内側は境界処理なしの単純な内積にして自動ベクトル化に載せる。
    if start >= 0 && start + kernel.len() as isize - 1 <= last {
        let seg = &input[start as usize..start as usize + kernel.len()];
        return kernel.iter().zip(seg).map(|(k, s)| k * s).sum();
    }
    kernel
        .iter()
        .enumerate()
        .map(|(j, k)| {
            let i = (start + j as isize).clamp(0, last) as usize;
            k * input[i]
        })
        .sum()
}

fn gcd(a: u32, b: u32) -> u32 {
    let (mut a, mut b) = (a, b);
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a.max(1)
}

/// 正規化 sinc: `sin(pi x) / (pi x)`。
fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        let pix = std::f64::consts::PI * x;
        pix.sin() / pix
    }
}

/// Blackman 窓。`t` は -1..=1。
fn blackman(t: f64) -> f64 {
    if t.abs() > 1.0 {
        return 0.0;
    }
    // 標準の Blackman を [-1, 1] に写像。
    let a = std::f64::consts::PI * (t + 1.0);
    0.42 - 0.5 * a.cos() + 0.08 * (2.0 * a).cos()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: u32, secs: f64) -> Vec<f32> {
        let n = (rate as f64 * secs) as usize;
        (0..n)
            .map(|i| {
                (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() as f32
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f64 {
        if x.is_empty() {
            return 0.0;
        }
        (x.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>() / x.len() as f64).sqrt()
    }

    // --- 入力レベル ---

    #[test]
    fn silence_reads_as_zero() {
        let meter = LevelMeter::new();
        meter.record(&[0.0; 128]);
        assert_eq!(meter.level(), 0.0);
        assert_eq!(meter.display_level(), 0.0);
    }

    #[test]
    fn rms_matches_the_known_value_for_a_sine() {
        // 正弦の RMS は振幅 / sqrt(2)。
        let meter = LevelMeter::new();
        let samples: Vec<f32> = (0..1_000)
            .map(|i| (2.0 * std::f32::consts::PI * i as f32 / 100.0).sin() * 0.5)
            .collect();
        meter.record(&samples);
        assert!((meter.level() - 0.353).abs() < 0.01, "rms={}", meter.level());
    }

    #[test]
    fn an_empty_chunk_keeps_the_previous_level() {
        let meter = LevelMeter::new();
        meter.record(&[0.5; 16]);
        let before = meter.level();
        meter.record(&[]);
        assert_eq!(meter.level(), before, "空チャンクで 0 に落ちた");
    }

    #[test]
    fn display_level_spreads_speech_across_the_range() {
        // 生の RMS をそのまま出すと通常の発話でメーターがほぼ振れない。
        assert_eq!(normalize_level(0.0), 0.0);
        let quiet = normalize_level(0.002); // 静かな環境音 (-54dB)
        let speech = normalize_level(0.05); // 通常の発話 (-26dB)
        let loud = normalize_level(0.5); // 大声 (-6dB)
        assert!(quiet < speech, "{quiet} < {speech}");
        assert!(speech < loud, "{speech} < {loud}");
        assert!(speech > 0.4 && speech < 0.8, "発話が中央付近に来ない: {speech}");
        // 0.5 は -6.02dB で上限のわずかに下。ほぼ振り切っていればよい。
        assert!(loud > 0.99, "大声で振り切らない: {loud}");
    }

    #[test]
    fn display_level_is_clamped_and_safe() {
        // 上下限を越えても 0..=1 から出ない。NaN や負値でも落ちない。
        assert_eq!(normalize_level(10.0), 1.0);
        assert_eq!(normalize_level(1e-9), 0.0);
        assert_eq!(normalize_level(-1.0), 0.0);
        // 非有限値は「レベル不明」。メーターを振り切らせるより黙らせる方が安全。
        assert_eq!(normalize_level(f32::NAN), 0.0);
        assert_eq!(normalize_level(f32::INFINITY), 0.0);
    }

    #[test]
    fn identity_when_rates_match() {
        let input = sine(440.0, 16_000, 0.1);
        assert_eq!(resample_mono(&input, 16_000, 16_000), input);
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert!(resample_mono(&[], 48_000, 16_000).is_empty());
    }

    #[test]
    fn downsample_48k_to_16k_preserves_length_and_level() {
        let input = sine(1_000.0, 48_000, 0.5);
        let out = resample_mono(&input, 48_000, 16_000);
        // 長さは 1/3。丸めで ±1 サンプルの誤差を許容。
        assert!((out.len() as isize - 8_000).abs() <= 1, "len={}", out.len());
        // 通過帯域の 1 kHz は振幅がほぼ保たれる (正弦の RMS = 1/sqrt(2))。
        let r = rms(&out);
        assert!((r - 0.707).abs() < 0.03, "rms={r}");
    }

    #[test]
    fn downsample_rejects_out_of_band_content() {
        // 10 kHz は 16 kHz 出力のナイキスト (8 kHz) を超える。
        // ローパスが効いていなければ 6 kHz へ折り返して残ってしまう。
        let input = sine(10_000.0, 48_000, 0.5);
        let out = resample_mono(&input, 48_000, 16_000);
        let r = rms(&out);
        assert!(r < 0.02, "帯域外成分が残っている: rms={r}");
    }

    #[test]
    fn downsample_44100_to_16k_is_supported() {
        let input = sine(1_000.0, 44_100, 0.25);
        let out = resample_mono(&input, 44_100, 16_000);
        assert!((out.len() as isize - 4_000).abs() <= 1, "len={}", out.len());
        assert!((rms(&out) - 0.707).abs() < 0.03);
    }

    #[test]
    fn upsample_is_supported() {
        let input = sine(1_000.0, 8_000, 0.25);
        let out = resample_mono(&input, 8_000, 16_000);
        assert!((out.len() as isize - 4_000).abs() <= 1, "len={}", out.len());
        assert!((rms(&out) - 0.707).abs() < 0.03);
    }

    /// 実機のマイクを 1 秒使う疎通確認。CI では動かせないので `#[ignore]`。
    ///
    /// 実行: `cargo test -- --ignored --nocapture live_capture_produces_wav`
    #[test]
    #[ignore = "実機の入力デバイスが必要"]
    fn live_capture_produces_wav() {
        let (limit_tx, _limit_rx) = crossbeam_channel::bounded(1);
        let recorder = match start(limit_tx) {
            Ok(r) => r,
            Err(e) => panic!("録音を開始できませんでした: {e}"),
        };
        println!("device = {}", recorder.device_name());
        std::thread::sleep(Duration::from_millis(1000));
        let (wav, duration) = finish(recorder).expect("録音の確定に失敗");
        println!("wav = {} bytes / {:.3} 秒", wav.len(), duration.as_secs_f64());

        assert_eq!(&wav[0..4], b"RIFF");
        // 1 秒 ±20% の実尺が取れていること。
        assert!(
            (0.8..1.2).contains(&duration.as_secs_f64()),
            "録音長が想定外: {duration:?}"
        );
        let spec = hound::WavReader::new(Cursor::new(wav.clone()))
            .expect("WAV 読み戻しに失敗")
            .spec();
        assert_eq!(spec.sample_rate, TARGET_SAMPLE_RATE);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.bits_per_sample, 16);
        // 16kHz mono 16bit → 1 秒あたり 32000 バイト前後。
        assert!(wav.len() > 25_000, "WAV が短すぎる: {} bytes", wav.len());
    }

    /// 位相テーブル経路とフォールバック経路が同じ結果を出すこと。
    ///
    /// `MAX_PHASES` を跨ぐ入力を実際に作るのは大きすぎるので、
    /// 同じ計算を両経路の式で組み立てて突き合わせる。
    #[test]
    fn phase_table_and_fallback_paths_agree() {
        let input = sine(1_000.0, 44_100, 0.05);
        let (src, dst) = (44_100u32, 16_000u32);
        let table_out = resample_mono(&input, src, dst);

        // フォールバック経路と同じ手順を手で再現する。
        let ratio = dst as f64 / src as f64;
        let design = SincDesign::new(ratio);
        let taps = design.taps();
        let g = gcd(src, dst);
        let phases = (dst / g) as u64;
        let step = (src / g) as u64;
        let out_len = ((input.len() as f64) * ratio).round().max(1.0) as usize;
        let mut kernel = vec![0f32; taps];
        let mut manual = Vec::with_capacity(out_len);
        for n in 0..out_len {
            let pos = n as u64 * step;
            let base = (pos / phases) as isize;
            let frac = (pos % phases) as f64 / phases as f64;
            design.write_kernel(frac, &mut kernel);
            manual.push(convolve(&input, base - design.half_width_i, &kernel));
        }

        assert_eq!(table_out.len(), manual.len());
        for (i, (a, b)) in table_out.iter().zip(&manual).enumerate() {
            assert!((a - b).abs() < 1e-6, "sample {i}: {a} vs {b}");
        }
    }

    /// 44.1k→16k の位相数が想定どおり小さいこと (テーブル経路に載る)。
    #[test]
    fn realistic_device_rates_use_the_phase_table() {
        for src in [8_000u32, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 96_000] {
            let phases = 16_000 / gcd(src, 16_000);
            assert!(
                (phases as usize) <= MAX_PHASES,
                "src={src} の位相数 {phases} がテーブル上限を超える"
            );
        }
    }

    /// M-2 の性能回帰。1 分の録音 (48kHz) の変換にかかる時間を測る。
    ///
    /// 目安は release で 100ms 以下。debug ビルドでは 40 倍近く遅いので
    /// 判定は release のみ (`cargo test --release -- --ignored`)。
    /// debug で走らせた場合は計測値の表示だけ行う。
    #[test]
    #[ignore = "計測用。release ビルドで実行すること"]
    fn resample_one_minute_is_fast() {
        let input = sine(1_000.0, 48_000, 60.0);
        let t0 = std::time::Instant::now();
        let out = resample_mono(&input, 48_000, 16_000);
        let elapsed = t0.elapsed();
        println!(
            "1 分 (48kHz {} サンプル) → 16kHz {} サンプル: {:?}",
            input.len(),
            out.len(),
            elapsed
        );
        #[cfg(not(debug_assertions))]
        assert!(
            elapsed < Duration::from_millis(100),
            "リサンプルが遅すぎる: {elapsed:?}"
        );
    }

    #[test]
    fn encode_wav_writes_valid_riff() {
        let samples = sine(440.0, 16_000, 0.05);
        let wav = encode_wav(&samples, 16_000).expect("WAV エンコードに失敗");
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        // ヘッダ + 16bit mono の本体。
        assert_eq!(wav.len(), 44 + samples.len() * 2);

        let reader = hound::WavReader::new(Cursor::new(wav)).expect("WAV 読み戻しに失敗");
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.spec().sample_rate, 16_000);
        assert_eq!(reader.spec().bits_per_sample, 16);
    }

    #[test]
    fn encode_wav_clamps_out_of_range_samples() {
        let wav = encode_wav(&[2.0, -2.0, 0.0], 16_000).expect("WAV エンコードに失敗");
        let samples: Vec<i16> = hound::WavReader::new(Cursor::new(wav))
            .expect("WAV 読み戻しに失敗")
            .into_samples::<i16>()
            .filter_map(|s| s.ok())
            .collect();
        assert_eq!(samples, vec![i16::MAX, -i16::MAX, 0]);
    }
}
