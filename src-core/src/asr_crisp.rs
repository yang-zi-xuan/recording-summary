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
///     { "offsets": { "from": 0, "to": 30000 }, "text": "…" }
///   ]
/// }
/// ```
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
}

#[derive(Debug, Default, Deserialize)]
struct CrispOffsets {
    #[serde(default)]
    from: u64,
    #[serde(default)]
    to: u64,
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

        let segments: Vec<Segment> = parsed
            .transcription
            .into_iter()
            .filter_map(|s| {
                let t = s.text.trim().to_string();
                if t.is_empty() {
                    return None;
                }
                Some(Segment::new(s.offsets.from, s.offsets.to, t))
            })
            .collect();

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
              "text": "第一段" },
            { "offsets": {"from": 30000, "to": 60000}, "text": "第二段" }
          ]
        }"#;
        let j: CrispJson = serde_json::from_str(raw).unwrap();
        assert_eq!(j.transcription.len(), 2);
        assert_eq!(j.transcription[0].offsets.from, 0);
        assert_eq!(j.transcription[0].offsets.to, 30000);
        assert_eq!(j.transcription[1].text, "第二段");
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
