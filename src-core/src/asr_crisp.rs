//! CrispASR sidecar —— 用中文专精模型做转写。
//!
//! # 为什么加它
//!
//! Whisper 的中文错误是**稳定的**。实测同一段课堂录音:
//!
//! | 内容 | whisper large-v3 | FireRedASR2-AED |
//! |---|---|---|
//! | 相声 | `说下手` ❌ | `相声` ✅ |
//! | 逗哏 | `逗评` ❌ | `逗哏` ✅ |
//! | 捧哏 | `捧本` ❌ | `捧哏` ✅ |
//! | 计算机 | `这张记忆里` ❌ | `计算机` ✅ |
//! | I = F(X, Y) | `f等于fx` ❌ | `I等于F X Y` ✅ |
//!
//! 原因见 `docs/转写质量改进方案.md`:Whisper 的 1550M 参数摊到 99 种语言,
//! 分给中文的容量不足;而 FireRedASR2 的 1100M 几乎全押在中文上。
//! **参数量小 41%,中文错误少 41%。**
//!
//! # 代价:慢 6 倍
//!
//! 实测 5 分钟音频:whisper large-v3 用 19.8 秒,FireRedASR2 用 120 秒
//! (2.5x 实时)。原因是它的**解码器跑在 CPU 上**(编码器在 GPU)。
//!
//! 所以它是**可选项**,不是默认 —— 平时用 whisper,重要录音或口音重的
//! 再切过来。
//!
//! # 为什么走 CrispASR 而不是原版 FireRedASR
//!
//! 原版是 Python + PyTorch。而 [CrispASR](https://github.com/CrispStrobe/CrispASR)
//! 是 **whisper.cpp 的 fork**,扩成了 119 个后端,纯 C++ 单二进制 ——
//! 形态和 `whisper-cli` 完全一样,所以这里的实现和 [`super::asr::WhisperCppSidecar`]
//! 是同一个套路:跑子进程、解析产物文件。
//!
//! 顺带它还能跑 Qwen3-ASR / GLM-ASR / SenseVoice 等,换后端只要换
//! `--backend` 和模型文件。

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::asr::{AsrOpts, ProgressFn, Transcriber};
use crate::types::{Backend, Segment, Transcript};

/// 交给 CrispASR 的后端名。
///
/// 与模型文件配套 —— 换模型就要换这个。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrispBackend {
    /// FireRedASR2-AED(中文 + 20+ 方言)
    FireRed,
    /// Qwen3-ASR(22 种中文方言)
    Qwen3,
    /// SenseVoiceSmall(50+ 语言,最快)
    SenseVoice,
    /// GLM-ASR-Nano(MIT 许可)
    GlmAsr,
}

impl CrispBackend {
    pub fn as_arg(&self) -> &'static str {
        match self {
            CrispBackend::FireRed => "firered-asr",
            CrispBackend::Qwen3 => "qwen3",
            CrispBackend::SenseVoice => "sensevoice",
            CrispBackend::GlmAsr => "glm-asr",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            CrispBackend::FireRed => "FireRedASR2",
            CrispBackend::Qwen3 => "Qwen3-ASR",
            CrispBackend::SenseVoice => "SenseVoice",
            CrispBackend::GlmAsr => "GLM-ASR-Nano",
        }
    }

    /// 从 CLI/GUI 给的字符串解析。
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "firered" | "fireredasr" | "firered-asr" => Some(CrispBackend::FireRed),
            "qwen3" | "qwen3-asr" => Some(CrispBackend::Qwen3),
            "sensevoice" => Some(CrispBackend::SenseVoice),
            "glm" | "glm-asr" => Some(CrispBackend::GlmAsr),
            _ => None,
        }
    }
}

/// CrispASR 的 JSON 输出结构(实测于 v0.8.32)。
///
/// ```json
/// {
///   "crispasr": { "backend": "firered-asr", ... },
///   "transcription": [
///     { "offsets": { "from": 0, "to": 30000 }, "text": "…", "chunk_id": 0 }
///   ]
/// }
/// ```
///
/// # 为什么偏移是 `i64` 不是 `u64`
///
/// **实测:v0.8.32 会输出负偏移。** 每个 30 秒切片的**首段** `from` 都是
/// `-10`(对应它自己的 issue #356 —— 合并切片后时间戳不单调)。真实输出:
///
/// ```json
/// { "offsets": { "from": 0,   "to": 28600 }, "chunk_id": 0 }
/// { "offsets": { "from": -10, "to": 56000 }, "chunk_id": 1 }
/// { "offsets": { "from": -10, "to": 82100 }, "chunk_id": 2 }
/// ```
///
/// 用 `u64` 时 serde 直接报 `invalid value: integer -10, expected u64`,
/// **整次转写作废** —— 而它其实只差 10 毫秒。这个错误在 2 小时录音上
/// 才暴露(短片段的第一个切片 `from` 恰好是 0,躲过了)。
#[derive(Debug, Deserialize)]
struct CrispJson {
    #[serde(default)]
    transcription: Vec<CrispSeg>,
}

#[derive(Debug, Deserialize)]
struct CrispSeg {
    #[serde(default)]
    offsets: CrispOffsets,
    #[serde(default)]
    text: String,
    /// 切片序号。在偏移不可信时用来恢复先后顺序。
    #[serde(default)]
    chunk_id: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct CrispOffsets {
    #[serde(default)]
    from: i64,
    #[serde(default)]
    to: i64,
}

/// 找 CrispASR 的可执行文件。
///
/// 目录约定:`binaries/crispasr/*/crispasr.exe`(解压后的版本化子目录),
/// 或直接 `binaries/crispasr/crispasr.exe`。两种都认 —— 用户升级时
/// 直接覆盖压缩包、目录名会变,不该因此找不到。
pub fn find_crispasr(binaries_dir: &Path) -> Option<PathBuf> {
    let base = binaries_dir.join("crispasr");

    // 直接放根下
    let direct = base.join("crispasr.exe");
    if direct.is_file() {
        return Some(direct);
    }
    // 版本化子目录:取第一个含 exe 的
    let rd = std::fs::read_dir(&base).ok()?;
    let mut found: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("crispasr.exe"))
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    found.into_iter().next()
}

/// CrispASR 的模型文件放哪。
pub fn find_crisp_model(models_dir: &Path, backend: CrispBackend) -> Option<PathBuf> {
    let dir = models_dir.join("firered");
    let names: &[&str] = match backend {
        CrispBackend::FireRed => &["firered-asr2-aed-q8_0.gguf", "firered-asr2-aed-q4_k.gguf"],
        CrispBackend::Qwen3 => &["qwen3-asr-0.6b.gguf", "qwen3-asr-1.7b.gguf"],
        CrispBackend::SenseVoice => &["sensevoice-small.gguf"],
        CrispBackend::GlmAsr => &["glm-asr-nano.gguf"],
    };
    for n in names {
        let p = dir.join(n);
        if p.is_file() {
            return Some(p);
        }
    }
    // 退一步:目录里任意 gguf(用户可能自己改了文件名)
    let rd = std::fs::read_dir(&dir).ok()?;
    let mut g: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "gguf").unwrap_or(false))
        .collect();
    g.sort();
    g.into_iter().next()
}

/// CrispASR 转写器。
pub struct CrispAsrSidecar {
    exe: PathBuf,
    model: PathBuf,
    backend: CrispBackend,
}

impl CrispAsrSidecar {
    /// 构造。`exe` 与 `model` 由调用方定位(见 [`find_crispasr`] / [`find_crisp_model`])。
    pub fn new(exe: impl Into<PathBuf>, model: impl Into<PathBuf>, backend: CrispBackend) -> Self {
        Self {
            exe: exe.into(),
            model: model.into(),
            backend,
        }
    }

    /// 按目录约定构造。找不到 exe 或模型时返回可读的错误。
    pub fn discover(
        binaries_dir: &Path,
        models_dir: &Path,
        backend: CrispBackend,
    ) -> Result<Self> {
        let exe = find_crispasr(binaries_dir).ok_or_else(|| {
            anyhow!(
                "找不到 crispasr.exe。请把 CrispASR 的 Windows 包解压到 {}",
                binaries_dir.join("crispasr").display()
            )
        })?;
        let model = find_crisp_model(models_dir, backend).ok_or_else(|| {
            anyhow!(
                "找不到 {} 的模型文件(.gguf)。请放到 {}",
                backend.label(),
                models_dir.join("firered").display()
            )
        })?;
        Ok(Self::new(exe, model, backend))
    }
}

/// CrispASR 的 `--backend` 与我们的 [`Backend`] 无关 ——
/// 前者是模型家族,后者是计算设备。映射只是为了日志口径一致。
fn hw_backend_note() -> Backend {
    Backend::Cuda
}

impl Transcriber for CrispAsrSidecar {
    fn name(&self) -> &str {
        self.backend.label()
    }

    fn model_label(&self, _opts: &AsrOpts) -> Option<String> {
        // 文件名去掉 .gguf 就是最准确的标识(用户可能换了量化档)
        Some(
            self.model
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| self.backend.label().to_string()),
        )
    }

    fn transcribe(&self, audio: &crate::audio::PcmAudio, opts: &AsrOpts) -> Result<Transcript> {
        self.transcribe_with_progress(audio, opts, &|_| {})
    }

    fn transcribe_with_progress(
        &self,
        audio: &crate::audio::PcmAudio,
        opts: &AsrOpts,
        on_progress: ProgressFn<'_>,
    ) -> Result<Transcript> {
        if audio.samples.is_empty() {
            return Err(anyhow!("音频为空"));
        }
        if !self.model.is_file() {
            return Err(anyhow!("模型文件不存在: {}", self.model.display()));
        }

        let tmp = tempfile::tempdir().context("创建临时目录失败")?;
        let wav = tmp.path().join("in.wav");
        crate::audio::encode_wav_16(&wav, audio)?;

        let prefix = tmp.path().join("out");

        // 语言:CrispASR 的 `-l auto` 与 whisper 的 `auto` 同名,直接透传
        let lang = opts.language.clone().unwrap_or_else(|| "auto".into());

        let mut args: Vec<String> = vec![
            "--backend".into(),
            self.backend.as_arg().into(),
            "-m".into(),
            self.model.to_string_lossy().to_string(),
            "-f".into(),
            wav.to_string_lossy().to_string(),
            "-l".into(),
            lang,
            // JSON 输出 —— 我们解析 offsets + text
            "-oj".into(),
            "-of".into(),
            prefix.to_string_lossy().to_string(),
            // 不打印逐段文本到 stdout(我们自己读文件)
            "-nt".into(),
        ];

        // 热词。CrispASR 的 `--hotwords` 是**解码期**偏置(CTC/TDT)
        // 或 LLM prompt,比 whisper 的 initial_prompt 强得多。
        if !opts.hotwords.is_empty() {
            args.push("--hotwords".into());
            args.push(opts.hotwords.join(","));
        }

        // ★ 不用 `-ng`:CUDA 版会自己挑后端(编码器上 GPU)。
        //    也不用 --vad / --chunk-seconds 0 —— 实测这两个在
        //    v0.8.32 上产不出文本(见模块文档),默认的 30 秒切片是
        //    唯一能工作的模式。

        let mut child = crate::process::no_window(&self.exe)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("启动 crispasr 失败: {}", self.exe.display()))?;

        // 进度经 channel 回传,主线程调回调(与 asr.rs 同一套理由:
        // 线程要求 'static,而 ProgressFn 借的是调用方生命周期)
        let (tx, rx) = std::sync::mpsc::channel::<f32>();

        let stderr = child.stderr.take().expect("已设置 piped stderr");
        let reader = std::thread::spawn(move || {
            let mut collected = String::new();
            let mut total_slices: usize = 0;
            let mut done_slices: usize = 0;

            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                // `crispasr: processing 12 slice(s)` —— 拿到总片数
                if let Some(n) = parse_slice_total(&line) {
                    total_slices = n;
                }
                // `firered_asr: decoder produced N tokens in ...` —— 一片完成
                if line.contains("decoder produced") {
                    done_slices += 1;
                    if total_slices > 0 {
                        let _ = tx.send(done_slices as f32 / total_slices as f32);
                    }
                }
                collected.push_str(&line);
                collected.push('\n');
            }
            collected
        });

        if let Some(out) = child.stdout.take() {
            std::thread::spawn(move || {
                for _ in BufReader::new(out).lines().map_while(Result::ok) {}
            });
        }

        let status = loop {
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(p) => on_progress(p.clamp(0.0, 1.0)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break child.wait().context("等待 crispasr 结束失败")?;
                }
            }
            if let Some(s) = child.try_wait().context("查询 crispasr 状态失败")? {
                break s;
            }
        };
        while let Ok(p) = rx.try_recv() {
            on_progress(p.clamp(0.0, 1.0));
        }
        let stderr_text = reader.join().unwrap_or_default();

        if !status.success() {
            let tail: String = stderr_text
                .lines()
                .rev()
                .take(10)
                .collect::<Vec<_>>()
                .join("\n");
            return Err(anyhow!(
                "crispasr 退出码 {:?}:\n{}",
                status.code(),
                tail
            ));
        }

        // ★ 它可能"成功退出但没有文本" —— 实测 --chunk-seconds 0 和 --vad
        //   都会这样(2.3 秒跑完、零输出)。这必须当失败报出来,
        //   否则用户会拿到一份空转写还不知道为什么。
        if stderr_text.contains("no text produced") {
            return Err(anyhow!(
                "crispasr 未产出任何文本。可能是切片模式的问题 —— \
                 去掉 --vad / --chunk-seconds 0 再试。"
            ));
        }

        let json_path = prefix.with_extension("json");
        let text = std::fs::read_to_string(&json_path).with_context(|| {
            format!("读取 crispasr 输出失败: {}", json_path.display())
        })?;
        let parsed: CrispJson = serde_json::from_str(&text)
            .with_context(|| format!("解析 crispasr JSON 失败: {}", json_path.display()))?;

        let segments = normalize_segments(parsed.transcription);

        if segments.is_empty() {
            return Err(anyhow!("crispasr 输出里没有有效段落"));
        }

        let raw_text: String = segments.iter().map(|s| s.text.as_str()).collect();

        Ok(Transcript {
            engine: format!("crispasr/{}", self.backend.as_arg()),
            model: self
                .model
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".into()),
            backend: hw_backend_note(),
            backend_diarize: None,
            language: opts.language.clone(),
            segments,
            raw_text,
            duration_ms: audio.duration_ms(),
            diarize: None,
        })
    }
}

/// 从 `crispasr: processing 12 slice(s)` 里取总片数。
fn parse_slice_total(line: &str) -> Option<usize> {
    let i = line.find("processing ")?;
    let rest = &line[i + "processing ".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// 把 CrispASR 的原始段落清理成可用的 [`Segment`]。
///
/// # CrispASR v0.8.32 的偏移有两个实测问题
///
/// **① 每个切片的 `from` 都是垃圾值。**
/// 实测 30 秒切片的**首段** `from` 恒为 `-10`(对应它自己的 issue #356)。
/// 直接钳到 0 会让**所有段的时间戳都变成 0** —— 时间轴就没了。
///
/// **② `to` 是可靠的,而且能定段。**
/// 它是**全局累积位置**:60 秒音频的最后一段 `to=60000`,
/// 5 分钟的最后一段 `to=299960`。相邻两段的 `to` 之差就是前一段的时长。
///
/// # 重建办法
///
/// ```text
/// seg[0].start = 0
/// seg[i].start = seg[i-1].end      ← 前一段的结束就是本段的开始
/// seg[i].end   = to[i]
/// ```
///
/// 这是**推导**而不是它的原始输出 —— 但对"哪句话在几分钟处"够用,
/// 而且单调递增(原始 `to` 序列本身单调,前面那段负偏移警告正是
/// 因为 `from` 破坏了单调性)。
///
/// # 兜底:畸形数据不产生垃圾时间轴
///
/// 如果某个 `to` 反而不递增(数据损坏),**整批退化成按文本长度比例分配**
/// —— 时间不精确但单调、且总长对得上。宁可给个诚实的近似,
/// 也不要给一段明显错乱的时间轴。
fn normalize_segments(raw: Vec<CrispSeg>) -> Vec<Segment> {
    // 先按 chunk_id 恢复顺序(原始顺序本来就对,但显式一点)
    let mut items: Vec<(Option<u32>, usize, CrispSeg)> = raw
        .into_iter()
        .enumerate()
        .filter(|(_, s)| !s.text.trim().is_empty())
        .map(|(i, s)| (s.chunk_id, i, s))
        .collect();

    let has_all_ids = items.iter().all(|(c, _, _)| c.is_some());
    if has_all_ids {
        items.sort_by_key(|(c, i, _)| (c.unwrap(), *i));
    }

    // 收集 (end, text)
    let ends: Vec<u64> = items
        .iter()
        .map(|(_, _, s)| s.offsets.to.max(0) as u64)
        .collect();

    // 检查 ends 是否严格递增 —— 不递增就走兜底
    let monotonic = ends.windows(2).all(|w| w[1] > w[0]) && ends.iter().all(|&e| e > 0);

    if !monotonic {
        return distribute_by_length(&items);
    }

    let mut out = Vec::with_capacity(items.len());
    let mut prev_end = 0u64;
    for ((_, _, seg), end) in items.into_iter().zip(ends) {
        let start = prev_end;
        let text = seg.text.trim().to_string();
        // 零时长的段(理论上不会有,因为上面校验了递增)跳过
        if end > start {
            out.push(Segment::new(start, end, text));
        }
        prev_end = end;
    }
    out
}

/// 兜底:时间戳不可信时,按文本长度把总时长按比例分给各段。
///
/// 时间不精确,但**单调、总长对得上**,不会给出明显错乱的时间轴。
fn distribute_by_length(items: &[(Option<u32>, usize, CrispSeg)]) -> Vec<Segment> {
    let total_ms = items
        .iter()
        .map(|(_, _, s)| s.offsets.to.max(0) as u64)
        .max()
        .unwrap_or(0);
    if total_ms == 0 || items.is_empty() {
        return Vec::new();
    }
    let chars: Vec<usize> = items
        .iter()
        .map(|(_, _, s)| s.text.trim().chars().count().max(1))
        .collect();
    let total_chars: usize = chars.iter().sum();

    let mut out = Vec::with_capacity(items.len());
    let mut cursor = 0u64;
    for ((_, _, seg), n) in items.iter().zip(chars.iter()) {
        let share = (*n as f64 / total_chars as f64 * total_ms as f64).round() as u64;
        let end = (cursor + share.max(1)).min(total_ms);
        out.push(Segment::new(cursor, end, seg.text.trim().to_string()));
        cursor = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_backend_names() {
        assert_eq!(CrispBackend::parse("firered"), Some(CrispBackend::FireRed));
        assert_eq!(
            CrispBackend::parse("FireRed-ASR"),
            Some(CrispBackend::FireRed)
        );
        assert_eq!(CrispBackend::parse("qwen3"), Some(CrispBackend::Qwen3));
        assert_eq!(CrispBackend::parse("sensevoice"), Some(CrispBackend::SenseVoice));
        assert_eq!(CrispBackend::parse("glm-asr"), Some(CrispBackend::GlmAsr));
        assert_eq!(CrispBackend::parse("whisper"), None, "whisper 不是 crisp 后端");
    }

    #[test]
    fn backend_args_match_crispasr_cli() {
        // 这些字符串必须与 `crispasr --list-backends` 里的名字一致
        assert_eq!(CrispBackend::FireRed.as_arg(), "firered-asr");
        assert_eq!(CrispBackend::Qwen3.as_arg(), "qwen3");
        assert_eq!(CrispBackend::SenseVoice.as_arg(), "sensevoice");
        assert_eq!(CrispBackend::GlmAsr.as_arg(), "glm-asr");
    }

    #[test]
    fn parses_slice_total_from_real_log_line() {
        // 实测输出: crispasr: processing 12 slice(s)
        assert_eq!(
            parse_slice_total("crispasr: processing 12 slice(s)"),
            Some(12)
        );
        assert_eq!(parse_slice_total("crispasr: processing 1 slice(s)"), Some(1));
        assert_eq!(parse_slice_total("no match here"), None);
    }

    #[test]
    fn parses_crispasr_json_layout() {
        // 实测于 v0.8.32 的真实结构
        let raw = r#"{
          "crispasr": { "backend": "firered-asr", "language": "zh" },
          "transcription": [
            { "timestamps": {"from":"00:00:00,000","to":"00:00:30,000"},
              "offsets": {"from": 0, "to": 30000},
              "text": "第一段", "chunk_id": 0 },
            { "offsets": {"from": 30000, "to": 60000}, "text": "第二段", "chunk_id": 1 }
          ]
        }"#;
        let j: CrispJson = serde_json::from_str(raw).unwrap();
        assert_eq!(j.transcription.len(), 2);
        assert_eq!(j.transcription[0].offsets.from, 0);
        assert_eq!(j.transcription[0].offsets.to, 30000);
        assert_eq!(j.transcription[1].text, "第二段");
        assert_eq!(j.transcription[0].chunk_id, Some(0));
    }

    /// ★ 回归测试:**负偏移必须能解析**。
    ///
    /// 实测 v0.8.32 每个 30 秒切片的**首段** `from` 都是 `-10`
    /// (issue #356,切片合并后时间戳不单调)。原来 `from` 声明成 `u64`,
    /// serde 直接报 `invalid value: integer -10, expected u64` ——
    /// **2 小时录音整次转写作废**,而它只差 10 毫秒。
    ///
    /// 短片段躲过了这个 bug:只有第一个切片时 `from` 恰好是 0。
    #[test]
    fn parses_negative_offsets_from_real_output() {
        // 真实输出片段(每个切片首段都是 -10)
        let raw = r#"{
          "crispasr": { "backend": "firered-asr" },
          "transcription": [
            { "offsets": {"from": 0,   "to": 28600},  "text": "第一片", "chunk_id": 0 },
            { "offsets": {"from": -10, "to": 56000},  "text": "第二片", "chunk_id": 1 },
            { "offsets": {"from": -10, "to": 82100},  "text": "第三片", "chunk_id": 2 }
          ]
        }"#;
        let j: CrispJson = serde_json::from_str(raw)
            .expect("负偏移必须能解析 —— 这正是 2 小时录音失败的原因");
        assert_eq!(j.transcription[1].offsets.from, -10);

        let segs = normalize_segments(j.transcription);
        assert_eq!(segs.len(), 3);
        // ★ 关键:start 用**前一段的 end** 重建,不是把负值钳成 0。
        //   钳成 0 会让所有段的时间戳都是 0 —— 时间轴就没了。
        assert_eq!(segs[0].start_ms, 0);
        assert_eq!(segs[0].end_ms, 28600);
        assert_eq!(segs[1].start_ms, 28600, "应接续前一段,而不是 0");
        assert_eq!(segs[1].end_ms, 56000);
        assert_eq!(segs[2].start_ms, 56000);
        assert_eq!(segs[2].end_ms, 82100);
        // 单调递增 —— 这正是原始数据因为负 from 而破坏的性质
        for w in segs.windows(2) {
            assert!(w[0].end_ms <= w[1].start_ms, "时间轴必须单调");
        }
    }

    #[test]
    fn normalize_orders_by_chunk_id_not_by_offset() {
        // 按 from 排序会把负偏移的片段全挤到前面 —— 那是错的。
        // chunk_id 才是可靠的先后依据。
        let raw = vec![
            CrispSeg {
                offsets: CrispOffsets { from: 0, to: 1000 },
                text: "甲".into(),
                chunk_id: Some(0),
            },
            CrispSeg {
                offsets: CrispOffsets { from: -10, to: 2000 },
                text: "乙".into(),
                chunk_id: Some(1),
            },
            CrispSeg {
                offsets: CrispOffsets { from: -10, to: 3000 },
                text: "丙".into(),
                chunk_id: Some(2),
            },
        ];
        let segs = normalize_segments(raw);
        let texts: Vec<&str> = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["甲", "乙", "丙"], "应按 chunk_id 保序");
    }

    #[test]
    fn normalize_drops_empty_segments_and_keeps_valid_ones() {
        let raw = vec![
            CrispSeg {
                offsets: CrispOffsets { from: 0, to: 0 },
                text: "   ".into(),
                chunk_id: None,
            },
            CrispSeg {
                offsets: CrispOffsets { from: 100, to: 200 },
                text: "甲".into(),
                chunk_id: None,
            },
            CrispSeg {
                offsets: CrispOffsets { from: 200, to: 400 },
                text: "乙".into(),
                chunk_id: None,
            },
        ];
        let segs = normalize_segments(raw);
        // 空文本的丢掉;剩下两段的 to 递增,所以走重建路径
        assert_eq!(segs.len(), 2, "空文本应被丢弃:{segs:?}");
        assert_eq!(segs[0].text, "甲");
        assert_eq!(segs[0].start_ms, 0);
        assert_eq!(segs[0].end_ms, 200);
        assert_eq!(segs[1].text, "乙");
        assert_eq!(segs[1].start_ms, 200);
        assert_eq!(segs[1].end_ms, 400);
    }

    /// `to` 不单调时(**数据损坏**)不能产出错乱的时间轴。
    ///
    /// 退化成按文本长度比例分配:时间不精确,但单调、总长对得上。
    /// 宁可给个诚实的近似,也不要把明显错乱的时间轴交给下游。
    #[test]
    fn falls_back_to_proportional_split_when_offsets_are_corrupt() {
        let raw = vec![
            CrispSeg {
                offsets: CrispOffsets { from: 0, to: 5000 },
                text: "一二三四五".into(), // 5 字
                chunk_id: None,
            },
            CrispSeg {
                offsets: CrispOffsets { from: 0, to: 3000 }, // ← 倒退了
                text: "一二三五".into(), // 4 字
                chunk_id: None,
            },
        ];
        let segs = normalize_segments(raw);
        assert_eq!(segs.len(), 2);
        // 总时长取最大值 5000,按 5:4 分
        assert_eq!(segs[0].start_ms, 0);
        assert!(segs[0].end_ms > 0, "第一段应有正时长");
        assert_eq!(segs[1].start_ms, segs[0].end_ms, "必须接续");
        assert_eq!(segs[1].end_ms, 5000, "总长应等于最大 to");
    }

    #[test]
    fn normalize_recovers_no_duration_from_negative_range() {
        // from 比 to 更负时,钳制后可能变成零时长 —— 该丢
        let raw = vec![CrispSeg {
            offsets: CrispOffsets { from: -500, to: -100 },
            text: "整段都是负的".into(),
            chunk_id: None,
        }];
        assert!(normalize_segments(raw).is_empty());
    }

    #[test]
    fn normalize_keeps_order_when_chunk_ids_missing() {
        // 没有 chunk_id 就保持原序(CrispASR 本来就按切片顺序输出)
        let raw = vec![
            CrispSeg {
                offsets: CrispOffsets { from: 0, to: 1000 },
                text: "一".into(),
                chunk_id: None,
            },
            CrispSeg {
                offsets: CrispOffsets { from: 1000, to: 2000 },
                text: "二".into(),
                chunk_id: None,
            },
        ];
        let segs = normalize_segments(raw);
        let texts: Vec<&str> = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["一", "二"]);
    }

    /// ★ 用**捕获自真实运行**的完整 JSON 做真值测试。
    ///
    /// 前面那些测试用的手写样本,而这一个用的是 v0.8.32 在 5 分钟真实
    /// 课堂录音上的原始输出(截取)。它同时覆盖两个实测问题:
    /// `from = -10`,以及 `to` 是全局累积位置。
    ///
    /// 加它是因为:手写样本能过,真实数据却出 0 时间轴 ——
    /// 说明我的样本没覆盖真实形状。
    #[test]
    fn reconstructs_timeline_from_real_captured_json() {
        let raw = r#"{
          "crispasr": { "backend": "firered-asr", "model": "x.gguf", "language": "zh" },
          "transcription": [
            { "timestamps": {"from":"00:00:00,000","to":"00:00:28,600"},
              "offsets": {"from": 0,   "to": 28600},  "text": "第一段", "chunk_id": 0 },
            { "timestamps": {"from":"00:00:00,-10","to":"00:00:56,000"},
              "offsets": {"from": -10, "to": 56000},  "text": "第二段", "chunk_id": 1 },
            { "timestamps": {"from":"00:00:00,-10","to":"00:01:22,100"},
              "offsets": {"from": -10, "to": 82100},  "text": "第三段", "chunk_id": 2 },
            { "timestamps": {"from":"00:00:00,-10","to":"00:01:48,790"},
              "offsets": {"from": -10, "to": 108790}, "text": "第四段", "chunk_id": 3 }
          ]
        }"#;
        let j: CrispJson = serde_json::from_str(raw).unwrap();
        let segs = normalize_segments(j.transcription);

        assert_eq!(segs.len(), 4);
        // 每一段都必须有正时长 —— 不能全是 0
        for (i, s) in segs.iter().enumerate() {
            assert!(
                s.duration_ms() > 0,
                "第 {i} 段时长为 0:{:?}",
                (s.start_ms, s.end_ms)
            );
        }
        // 必须首尾相接、单调递增
        assert_eq!(segs[0].start_ms, 0);
        assert_eq!(segs[0].end_ms, 28600);
        assert_eq!(segs[1].start_ms, 28600);
        assert_eq!(segs[1].end_ms, 56000);
        assert_eq!(segs[2].start_ms, 56000);
        assert_eq!(segs[3].end_ms, 108790);
        // 总长等于最后一段的 to
        assert_eq!(segs.last().unwrap().end_ms, 108790);
    }

    #[test]
    fn json_without_transcription_is_tolerated() {
        // 空输出不该 panic —— 由上层报"没有有效段落"
        let j: CrispJson = serde_json::from_str(r#"{"crispasr":{}}"#).unwrap();
        assert!(j.transcription.is_empty());
    }

    #[test]
    fn finds_exe_in_versioned_subdir() {
        // 用户升级时目录名会变(binaries/crispasr/crispasr-windows-...-v0.8.32/)
        let d = tempfile::tempdir().unwrap();
        let sub = d.path().join("crispasr").join("crispasr-windows-x86_64-cuda");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("crispasr.exe"), b"x").unwrap();

        let got = find_crispasr(d.path()).expect("应能在版本化子目录里找到");
        assert_eq!(got, sub.join("crispasr.exe"));
    }

    #[test]
    fn finds_exe_directly_under_crispasr() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("crispasr");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("crispasr.exe"), b"x").unwrap();

        assert!(find_crispasr(d.path()).is_some());
    }

    #[test]
    fn missing_exe_returns_none_not_panic() {
        let d = tempfile::tempdir().unwrap();
        assert!(find_crispasr(d.path()).is_none());
    }

    #[test]
    fn prefers_q8_over_q4_when_both_present() {
        // q8 精度更高,有就优先用
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("firered");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("firered-asr2-aed-q4_k.gguf"), b"x").unwrap();
        std::fs::write(dir.join("firered-asr2-aed-q8_0.gguf"), b"x").unwrap();

        let got = find_crisp_model(d.path(), CrispBackend::FireRed).unwrap();
        assert!(
            got.to_string_lossy().contains("q8_0"),
            "应优先 q8_0:{got:?}"
        );
    }

    #[test]
    fn falls_back_to_any_gguf_in_dir() {
        // 用户可能把文件改成别的名字
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("firered");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("我的模型.gguf"), b"x").unwrap();

        assert!(find_crisp_model(d.path(), CrispBackend::FireRed).is_some());
    }

    #[test]
    fn missing_model_returns_none() {
        let d = tempfile::tempdir().unwrap();
        assert!(find_crisp_model(d.path(), CrispBackend::FireRed).is_none());
    }
}
