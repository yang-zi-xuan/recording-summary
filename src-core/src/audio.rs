//! 音频解码、重采样与切分。
//!
//! 约定:所有送进 ASR 的音频统一为 **16kHz 单声道 f32**。
//!
//! 解码策略:
//! - WAV:内置解析器,无需外部依赖(便于测试与最小可用路径)
//! - 其他格式(mp3/m4a/mp4/flac/...):调用 ffmpeg
//!
//! 切分(见技术方案 §3.4):长音频按块处理,块边界落在静音处,
//! 这样 CPU 模式下可以断点续传,也能避免把词切两半。

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

/// ASR 期望的采样率。
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// 一段内存中的音频。
#[derive(Clone, Debug)]
pub struct PcmAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}

impl PcmAudio {
    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate == 0 || self.channels == 0 {
            return 0;
        }
        let frames = self.samples.len() as u64 / self.channels as u64;
        frames * 1000 / self.sample_rate as u64
    }

    /// 降混为单声道。
    pub fn to_mono(&self) -> PcmAudio {
        if self.channels <= 1 {
            return self.clone();
        }
        let ch = self.channels as usize;
        let frames = self.samples.len() / ch;
        let mut out = Vec::with_capacity(frames);
        for i in 0..frames {
            let base = i * ch;
            let sum: f32 = self.samples[base..base + ch].iter().sum();
            out.push(sum / ch as f32);
        }
        PcmAudio {
            samples: out,
            sample_rate: self.sample_rate,
            channels: 1,
        }
    }

    /// 线性插值重采样到目标采样率(仅用于非关键路径;
    /// 生产级重采样应换成 rubato,见技术方案 §2.3)。
    pub fn resample_to(&self, target: u32) -> PcmAudio {
        if self.sample_rate == target || self.sample_rate == 0 {
            return self.clone();
        }
        let ratio = target as f64 / self.sample_rate as f64;
        let out_len = ((self.samples.len() as f64) * ratio).round() as usize;
        let mut out = Vec::with_capacity(out_len);
        for i in 0..out_len {
            let src = i as f64 / ratio;
            let i0 = src.floor() as usize;
            let i1 = (i0 + 1).min(self.samples.len().saturating_sub(1));
            let frac = (src - i0 as f64) as f32;
            let a = self.samples.get(i0).copied().unwrap_or(0.0);
            let b = self.samples.get(i1).copied().unwrap_or(a);
            out.push(a + (b - a) * frac);
        }
        PcmAudio {
            samples: out,
            sample_rate: target,
            channels: self.channels,
        }
    }

    /// 归一化到 ASR 期望格式:16kHz 单声道。
    pub fn to_asr_format(&self) -> PcmAudio {
        self.to_mono().resample_to(TARGET_SAMPLE_RATE)
    }

    /// 峰值电平,用于质量评估(声纹登记的质量门槛)。
    pub fn peak(&self) -> f32 {
        self.samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    /// 粗略 RMS,作为信噪比的替代指标。
    pub fn rms(&self) -> f32 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let sum: f32 = self.samples.iter().map(|s| s * s).sum();
        (sum / self.samples.len() as f32).sqrt()
    }

    /// 截取时间区间(毫秒)。
    pub fn slice_ms(&self, start_ms: u64, end_ms: u64) -> PcmAudio {
        let sr = self.sample_rate as u64;
        let ch = self.channels as u64;
        let s = ((start_ms * sr / 1000) * ch).min(self.samples.len() as u64) as usize;
        let e = ((end_ms * sr / 1000) * ch).min(self.samples.len() as u64) as usize;
        PcmAudio {
            samples: self.samples[s..e.max(s)].to_vec(),
            sample_rate: self.sample_rate,
            channels: self.channels,
        }
    }
}

// ---------------------------------------------------------------------------
// WAV 解析(无外部依赖路径)
// ---------------------------------------------------------------------------

/// 极简 WAV 解析器,支持 PCM 16/24/32 位与 IEEE float32。
///
/// 只覆盖解码所需的最小集合 —— 复杂容器交给 ffmpeg。
pub fn decode_wav(path: &Path) -> Result<PcmAudio> {
    let bytes = std::fs::read(path).with_context(|| format!("读取 WAV 失败: {}", path.display()))?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(anyhow!("不是合法的 WAV 文件: {}", path.display()));
    }

    let mut pos = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // (format, channels, rate, bits)
    let mut data: Option<&[u8]> = None;

    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body_start = pos + 8;
        let body_end = (body_start + size).min(bytes.len());

        match id {
            b"fmt " => {
                if size < 16 {
                    return Err(anyhow!("WAV fmt 块过短"));
                }
                let b = &bytes[body_start..body_end];
                let format = u16::from_le_bytes([b[0], b[1]]);
                let channels = u16::from_le_bytes([b[2], b[3]]);
                let rate = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
                let bits = u16::from_le_bytes([b[14], b[15]]);
                fmt = Some((format, channels, rate, bits));
            }
            b"data" => data = Some(&bytes[body_start..body_end]),
            _ => {}
        }

        // 块按偶数字节对齐
        pos = body_start + size + (size & 1);
    }

    let (format, channels, rate, bits) =
        fmt.ok_or_else(|| anyhow!("WAV 缺少 fmt 块: {}", path.display()))?;
    let data = data.ok_or_else(|| anyhow!("WAV 缺少 data 块: {}", path.display()))?;
    if channels == 0 || rate == 0 {
        return Err(anyhow!("WAV 通道数/采样率非法"));
    }

    let samples: Vec<f32> = match (format, bits) {
        // PCM 整数
        (1, 16) => data
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
            .collect(),
        (1, 24) => data
            .chunks_exact(3)
            .map(|c| {
                let v = ((c[2] as i32) << 16) | ((c[1] as i32) << 8) | (c[0] as i32);
                let v = if v & 0x0080_0000 != 0 { v | !0x00FF_FFFF } else { v };
                v as f32 / 8_388_608.0
            })
            .collect(),
        (1, 32) => data
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2_147_483_648.0)
            .collect(),
        (1, 8) => data.iter().map(|&b| (b as f32 - 128.0) / 128.0).collect(),
        // IEEE float
        (3, 32) => data
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
        (3, 64) => data
            .chunks_exact(8)
            .map(|c| {
                f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]) as f32
            })
            .collect(),
        (f, b) => {
            return Err(anyhow!(
                "暂不支持的 WAV 格式: format={f}, bits={b}(请走 ffmpeg 解码)"
            ))
        }
    };

    Ok(PcmAudio {
        samples,
        sample_rate: rate,
        channels,
    })
}

/// 写 16-bit PCM WAV —— 供测试与 sidecar 输入使用。
pub fn encode_wav_16(path: &Path, audio: &PcmAudio) -> Result<()> {
    let ch = audio.channels.max(1);
    let sr = audio.sample_rate;
    let bits = 16u16;
    let block_align = ch * bits / 8;
    let byte_rate = sr * block_align as u32;
    let data_len = (audio.samples.len() * 2) as u32;

    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&ch.to_le_bytes());
    out.extend_from_slice(&sr.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in &audio.samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, out).with_context(|| format!("写入 WAV 失败: {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ffmpeg 解码
// ---------------------------------------------------------------------------

/// ffmpeg 可执行文件解析顺序:
/// 1. 显式配置
/// 2. `RECSUM_FFMPEG` 环境变量
/// 3. `binaries/ffmpeg/` 随包分发
/// 4. PATH
pub fn find_ffmpeg(explicit: Option<&Path>, sidecar_root: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    if let Some(v) = std::env::var_os("RECSUM_FFMPEG") {
        let p = PathBuf::from(v);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(root) = sidecar_root {
        for name in ["ffmpeg.exe", "ffmpeg"] {
            let p = root.join("ffmpeg").join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    if let Some(p) = crate::hardware::find_in_path("ffmpeg") {
        return Some(p);
    }
    None
}

/// 用 ffmpeg 解码任意格式为 16kHz 单声道 f32。
pub fn decode_with_ffmpeg(ffmpeg: &Path, input: &Path) -> Result<PcmAudio> {
    // ★ 用 no_window —— 否则每解码一个文件就闪一个黑窗口
    let out = crate::process::no_window(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            &input.to_string_lossy(),
            "-f",
            "f32le",
            "-acodec",
            "pcm_f32le",
            "-ac",
            "1",
            "-ar",
            "16000",
            "pipe:1",
        ])
        .output()
        .with_context(|| format!("调用 ffmpeg 失败: {}", ffmpeg.display()))?;

    if !out.status.success() {
        return Err(anyhow!(
            "ffmpeg 解码失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    let samples: Vec<f32> = out
        .stdout
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();

    Ok(PcmAudio {
        samples,
        sample_rate: TARGET_SAMPLE_RATE,
        channels: 1,
    })
}

/// 统一入口:按扩展名选择解码路径,必要时回退到 ffmpeg。
pub fn decode_audio(
    path: &Path,
    ffmpeg: Option<&Path>,
    sidecar_root: Option<&Path>,
) -> Result<PcmAudio> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    if ext == "wav" {
        match decode_wav(path) {
            Ok(a) => return Ok(a.to_asr_format()),
            Err(e) => {
                // 非常规 WAV(如 ADPCM)退回 ffmpeg
                tracing::debug!("内置 WAV 解析失败,回退 ffmpeg: {e}");
            }
        }
    }

    let ff = find_ffmpeg(ffmpeg, sidecar_root).ok_or_else(|| {
        anyhow!(
            "需要 ffmpeg 来解码 .{ext} 文件,但未找到。\n\
             请安装 ffmpeg 并加入 PATH,或放到 binaries/ffmpeg/ 目录,\n\
             或设置环境变量 RECSUM_FFMPEG 指向可执行文件。"
        )
    })?;
    decode_with_ffmpeg(&ff, path)
}

// ---------------------------------------------------------------------------
// 切分
// ---------------------------------------------------------------------------

/// 一个待转写的音频块。
#[derive(Clone, Debug)]
pub struct Chunk {
    pub index: u32,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// 按目标时长切块,**块边界尽量落在静音处**(避免把词切两半)。
///
/// 返回的块覆盖整段音频且首尾相接。
pub fn plan_chunks(audio: &PcmAudio, target_chunk_ms: u64) -> Vec<Chunk> {
    let total = audio.duration_ms();
    if total == 0 {
        return vec![];
    }
    if total <= target_chunk_ms {
        return vec![Chunk {
            index: 0,
            start_ms: 0,
            end_ms: total,
        }];
    }

    let sr = audio.sample_rate as usize;
    let win_ms: u64 = 50;
    let win = (sr * win_ms as usize / 1000).max(1);

    let mut chunks = Vec::new();
    let mut cursor = 0u64;
    let mut index = 0u32;

    while cursor < total {
        let ideal_end = (cursor + target_chunk_ms).min(total);
        if ideal_end == total {
            chunks.push(Chunk {
                index,
                start_ms: cursor,
                end_ms: total,
            });
            break;
        }

        // 在理想边界附近的 ±20% 区间内找能量最低的窗口作为切点
        let search_ms = (target_chunk_ms / 5).max(500);
        let lo = ideal_end.saturating_sub(search_ms).max(cursor + 1);
        let hi = (ideal_end + search_ms).min(total);

        let mut best = ideal_end;
        let mut best_energy = f32::MAX;
        let mut t = lo;
        while t + win_ms <= hi {
            let s = (t as usize * sr / 1000).min(audio.samples.len());
            let e = (s + win).min(audio.samples.len());
            if e <= s {
                break;
            }
            let energy: f32 =
                audio.samples[s..e].iter().map(|x| x * x).sum::<f32>() / (e - s) as f32;
            if energy < best_energy {
                best_energy = energy;
                best = t + win_ms / 2;
            }
            t += win_ms;
        }

        chunks.push(Chunk {
            index,
            start_ms: cursor,
            end_ms: best,
        });
        cursor = best;
        index += 1;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(ms: u64, sr: u32, freq: f32) -> PcmAudio {
        let n = (sr as u64 * ms / 1000) as usize;
        let samples = (0..n)
            .map(|i| {
                let t = i as f32 / sr as f32;
                (2.0 * std::f32::consts::PI * freq * t).sin() * 0.5
            })
            .collect();
        PcmAudio {
            samples,
            sample_rate: sr,
            channels: 1,
        }
    }

    fn silence(ms: u64, sr: u32) -> PcmAudio {
        PcmAudio {
            samples: vec![0.0; (sr as u64 * ms / 1000) as usize],
            sample_rate: sr,
            channels: 1,
        }
    }

    #[test]
    fn duration_math() {
        let a = tone(1000, 16_000, 440.0);
        assert_eq!(a.duration_ms(), 1000);
    }

    #[test]
    fn stereo_downmix_averages_channels() {
        let a = PcmAudio {
            samples: vec![1.0, 0.0, 1.0, 0.0],
            sample_rate: 16_000,
            channels: 2,
        };
        let m = a.to_mono();
        assert_eq!(m.channels, 1);
        assert_eq!(m.samples, vec![0.5, 0.5]);
    }

    #[test]
    fn resample_preserves_duration() {
        let a = tone(1000, 48_000, 440.0);
        let r = a.resample_to(16_000);
        assert_eq!(r.sample_rate, 16_000);
        // 允许 1ms 误差
        assert!((r.duration_ms() as i64 - 1000).abs() <= 1);
    }

    #[test]
    fn to_asr_format_is_16k_mono() {
        let a = PcmAudio {
            samples: vec![0.1; 48_000 * 2],
            sample_rate: 48_000,
            channels: 2,
        };
        let f = a.to_asr_format();
        assert_eq!(f.sample_rate, 16_000);
        assert_eq!(f.channels, 1);
        assert_eq!(f.duration_ms(), 1000);
    }

    #[test]
    fn wav_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.wav");
        let a = tone(500, 16_000, 440.0);
        encode_wav_16(&p, &a).unwrap();
        let back = decode_wav(&p).unwrap();
        assert_eq!(back.sample_rate, 16_000);
        assert_eq!(back.channels, 1);
        assert_eq!(back.samples.len(), a.samples.len());
        // 16-bit 量化误差
        for (x, y) in a.samples.iter().zip(back.samples.iter()) {
            assert!((x - y).abs() < 1e-3, "{x} vs {y}");
        }
    }

    #[test]
    fn decode_wav_rejects_non_wav() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.wav");
        std::fs::write(&p, b"not a wav file at all").unwrap();
        assert!(decode_wav(&p).is_err());
    }

    #[test]
    fn chunking_covers_whole_audio() {
        let a = tone(10 * 60 * 1000, 16_000, 440.0);
        let chunks = plan_chunks(&a, 5 * 60 * 1000);
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0].start_ms, 0);
        assert_eq!(chunks.last().unwrap().end_ms, a.duration_ms());
        // 首尾相接、无空洞
        for w in chunks.windows(2) {
            assert_eq!(w[0].end_ms, w[1].start_ms, "块之间不能有空洞或重叠");
        }
        // 索引连续
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.index as usize, i);
        }
    }

    #[test]
    fn chunking_single_when_short() {
        let a = tone(1000, 16_000, 440.0);
        let chunks = plan_chunks(&a, 5 * 60 * 1000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].end_ms, 1000);
    }

    #[test]
    fn chunking_prefers_low_energy_boundary() {
        // 前 4 分钟有声,第 4~6 分钟静音,再 4 分钟有声
        let mut a = tone(4 * 60 * 1000, 16_000, 440.0);
        a.samples.extend(silence(2 * 60 * 1000, 16_000).samples);
        a.samples.extend(tone(4 * 60 * 1000, 16_000, 440.0).samples);
        let chunks = plan_chunks(&a, 5 * 60 * 1000);
        assert!(chunks.len() >= 2);
        // 第一个切点应落在静音区(4~6 分钟)
        let cut = chunks[0].end_ms;
        assert!(
            (4 * 60 * 1000..=6 * 60 * 1000).contains(&cut),
            "切点 {cut}ms 应落在静音区,而不是硬切在 {ideal}ms",
            ideal = 5 * 60 * 1000
        );
    }

    #[test]
    fn empty_audio_yields_no_chunks() {
        let a = PcmAudio {
            samples: vec![],
            sample_rate: 16_000,
            channels: 1,
        };
        assert!(plan_chunks(&a, 1000).is_empty());
    }

    #[test]
    fn peak_and_rms_on_silence() {
        let s = silence(100, 16_000);
        assert_eq!(s.peak(), 0.0);
        assert_eq!(s.rms(), 0.0);
        let t = tone(100, 16_000, 440.0);
        assert!(t.peak() > 0.4 && t.rms() > 0.1);
    }

    #[test]
    fn slice_ms_bounds_are_clamped() {
        let a = tone(1000, 16_000, 440.0);
        let s = a.slice_ms(900, 5000);
        assert!(s.duration_ms() <= 100);
        let s2 = a.slice_ms(5000, 6000);
        assert_eq!(s2.samples.len(), 0);
    }
}
