//! 转写引擎。见技术方案 §2.4 / §4.2。
//!
//! **决定用 sidecar(子进程调 `whisper-cli.exe`)而不是 FFI 链接**,理由:
//!
//! | | sidecar | FFI |
//! |---|---|---|
//! | 崩溃隔离 | ✅ 子进程崩了主程序没事 | ❌ 整个应用崩 |
//! | **多后端切换** | ✅ **换一个 exe 路径即可** | ❌ 要维护多套构建配置 |
//! | 进度反馈 | 解析 stderr | 回调 |
//!
//! 多后端那条是决定性的 —— 见技术方案 §3.5。

use crate::audio::{self, Chunk, PcmAudio};
use crate::hardware::SidecarLocator;
use crate::types::{Backend, Segment, Transcript};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// 转写选项。
///
/// ★ 中文场景会自动附加简体提示,见 [`AsrOpts::initial_prompt`]。
pub const SIMPLIFIED_HINT: &str = "以下是普通话的句子,请使用简体中文";

#[derive(Clone, Debug)]
pub struct AsrOpts {
    pub model_path: PathBuf,
    pub backend: Backend,
    /// None = 自动检测
    pub language: Option<String>,
    /// 术语表 → initial_prompt。注意 Whisper 是 30 秒滑窗,热词效果有限,
    /// 真正的术语校正在 LLM 那一环(见技术方案 §10.8)。
    pub hotwords: Vec<String>,
    pub vad: bool,
    pub threads: usize,
    /// 分块目标时长(毫秒)。长音频切块以便断点续传。
    pub chunk_target_ms: u64,
}

impl Default for AsrOpts {
    fn default() -> Self {
        Self {
            model_path: PathBuf::new(),
            backend: Backend::Cpu,
            language: None,
            hotwords: vec![],
            vad: true,
            threads: 0, // 0 = 自动
            chunk_target_ms: 5 * 60 * 1000,
        }
    }
}

impl AsrOpts {
    /// 热词表版本,进缓存键。
    pub fn hotwords_version(&self) -> u32 {
        if self.hotwords.is_empty() {
            return 0;
        }
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for w in &self.hotwords {
            h.update(w.as_bytes());
            h.update(b"\n");
        }
        let d = h.finalize();
        u32::from_le_bytes([d[0], d[1], d[2], d[3]])
    }

    /// 拼成 initial_prompt。
    ///
    /// # 简体中文提示:始终注入,不按语言判断
    ///
    /// Whisper 在中文音频上有稳定的繁体偏好(实测「同學們」「神經網絡」「習題」),
    /// 一句提示即可矫正,顺带还能降低同音字错误率。
    ///
    /// **但按 `language.starts_with("zh")` 判断是错的。** 语言检测 ≠ 内容语言:
    /// 一节中英混合的课(英文术语密集)会被判成 `en`,而中文部分照样要出简体。
    /// 结果是「自动检测」——这个最推荐、也最适合混合内容的选项——反而拿不到保护。
    ///
    /// **副作用实测可接受:** 纯英文音频加这句提示后内容完全正确,
    /// 只有一个词的句首大小写受影响(`Transformer` → `transformer`)。
    /// 拿这点代价换中文部分不出繁体,是划算的。
    ///
    /// Whisper 的限制是 n_text_ctx/2 token,中文大致 1 字 ≈ 1 token,这里保守截断。
    pub fn initial_prompt(&self) -> Option<String> {
        let mut parts: Vec<String> = vec![SIMPLIFIED_HINT.to_string()];
        if !self.hotwords.is_empty() {
            parts.push(self.hotwords.join("、"));
        }
        let joined = parts.join("。");
        let truncated: String = joined.chars().take(200).collect();
        Some(truncated)
    }

    pub fn effective_threads(&self) -> usize {
        if self.threads > 0 {
            self.threads
        } else {
            crate::hardware::cpu_cores().clamp(1, 16)
        }
    }
}

/// 转写进度回调。参数是 0.0~1.0 的比例。
///
/// **为什么需要它:** 一次转写可能跑几分钟(CPU 上 45 分钟的课尤其明显),
/// 没有进度的话界面上的进度条会一直停在 0%,直到突然跳到 100% ——
/// 用户无法判断是"在跑"还是"卡死了"。
///
/// 用 `&dyn Fn` 而不是泛型,是为了让 `Transcriber` 保持对象安全
/// (管线上持有的是 `&dyn Transcriber`)。
pub type ProgressFn<'a> = &'a (dyn Fn(f32) + Send + Sync);

/// 转写引擎接口。换引擎/换后端都不该影响上层。
pub trait Transcriber: Send + Sync {
    /// 转写。
    ///
    /// `on_progress` 在转写过程中被调用若干次(不保证频率)。
    /// 实现方可以忽略它(默认实现就是这么做的),但**不该**依赖它被调用。
    fn transcribe(&self, audio: &PcmAudio, opts: &AsrOpts) -> Result<Transcript>;

    /// 带进度的转写。
    ///
    /// 默认实现直接忽略回调 —— 这样简单的引擎(比如测试用的 MockTranscriber)
    /// 不必都去实现一遍进度上报。
    fn transcribe_with_progress(
        &self,
        audio: &PcmAudio,
        opts: &AsrOpts,
        _on_progress: ProgressFn<'_>,
    ) -> Result<Transcript> {
        self.transcribe(audio, opts)
    }

    fn name(&self) -> &str;
}

// ---------------------------------------------------------------------------
// whisper.cpp sidecar
// ---------------------------------------------------------------------------

/// whisper.cpp CLI 的 JSON 输出结构(实测于 v1.8.x)。
///
/// 注意输出里**同时有** `offsets`(毫秒整数)和 `timestamps`(字符串)。
/// **解析用 `offsets`** —— 字符串要处理区域设置与格式差异,整数更可靠。
#[derive(Debug, Deserialize)]
struct WhisperJson {
    #[serde(default)]
    result: WhisperResult,
    #[serde(default)]
    transcription: Vec<WhisperSeg>,
}

#[derive(Debug, Default, Deserialize)]
struct WhisperResult {
    #[serde(default)]
    language: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WhisperSeg {
    #[serde(default)]
    offsets: WhisperOffsets,
    #[serde(default)]
    text: String,
}

#[derive(Debug, Default, Deserialize)]
struct WhisperOffsets {
    #[serde(default)]
    from: u64,
    #[serde(default)]
    to: u64,
}

/// whisper-cli 的进度行:`whisper_print_progress_callback: progress =  25%`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscribeProgress {
    pub chunk_index: u32,
    pub chunk_count: u32,
    pub chunk_pct: f32,
    /// 已处理音频时长(毫秒)—— 比百分比更能让用户估算"还要多久"
    pub audio_ms_done: u64,
    pub audio_ms_total: u64,
}

/// 通过子进程调用 whisper.cpp。
pub struct WhisperCppSidecar {
    locator: SidecarLocator,
    /// 覆盖自动定位(测试用)
    explicit_bin: Option<PathBuf>,
}

impl WhisperCppSidecar {
    pub fn new(locator: SidecarLocator) -> Self {
        Self {
            locator,
            explicit_bin: None,
        }
    }

    pub fn with_bin(mut self, bin: impl Into<PathBuf>) -> Self {
        self.explicit_bin = Some(bin.into());
        self
    }

    pub fn resolve_bin(&self, backend: Backend) -> Result<PathBuf> {
        if let Some(b) = &self.explicit_bin {
            if b.is_file() {
                return Ok(b.clone());
            }
            return Err(anyhow!("指定的 whisper-cli 不存在: {}", b.display()));
        }
        self.locator.whisper_cli(backend).ok_or_else(|| {
            anyhow!(
                "未找到 {} 版 whisper-cli(期望位置 {})",
                backend.label(),
                self.locator.root.join(backend.sidecar_dir()).display()
            )
        })
    }

    /// 组装命令行参数。
    fn build_args(&self, wav: &Path, out_prefix: &Path, opts: &AsrOpts) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "-m".into(),
            opts.model_path.to_string_lossy().into_owned(),
            "-f".into(),
            wav.to_string_lossy().into_owned(),
            "-of".into(),
            out_prefix.to_string_lossy().into_owned(),
            "-oj".into(),   // 输出 JSON
            "-np".into(),   // 不打印额外内容
            "-pp".into(),   // 但要进度
            "-t".into(),
            opts.effective_threads().to_string(),
            "-l".into(),
            // ⚠️ whisper-cli 的 -l 默认是 "en",必须显式给值
            opts.language.clone().unwrap_or_else(|| "auto".into()),
        ];
        if let Some(p) = opts.initial_prompt() {
            a.push("--prompt".into());
            a.push(p);
        }
        // ⚠️ 明确不传 -ml / --max-len:字级时间戳在 CJK 上工作得很差,
        //    我们走段落级重叠投票(技术方案 §7.4)。
        a
    }

    /// 跑一次 whisper-cli。
    ///
    /// `on_progress` 收到的是 **0.0~1.0 的比例**。它由 whisper-cli 的
    /// `-pp` 输出解析而来 —— 那些行长这样:
    ///
    /// ```text
    /// whisper_print_progress_callback: progress =  26%
    /// ```
    ///
    /// **必须流式读 stderr。** 早先的实现用 `.output()` 等进程结束才拿到
    /// stderr,于是进度信息全被攒在最后一起到达 —— 界面上的进度条会一直
    /// 停在 0%。改成 `spawn` + 逐行读,进度就能实时穿上去。
    fn run_one(
        &self,
        bin: &Path,
        wav: &Path,
        out_prefix: &Path,
        opts: &AsrOpts,
        on_progress: ProgressFn<'_>,
    ) -> Result<WhisperJson> {
        use std::io::{BufRead, BufReader};

        let args = self.build_args(wav, out_prefix, opts);
        tracing::debug!(?args, "运行 whisper-cli");

        // ★ 用 no_window —— 否则每转一段就闪一个黑窗口
        let mut child = crate::process::no_window(bin)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("启动 whisper-cli 失败: {}", bin.display()))?;

        // 进度经 channel 回传,由**主线程**调用回调。
        //
        // 为什么不让读线程直接调 `on_progress`:`ProgressFn` 借的是调用方的
        // 生命周期,而线程要求 `'static` —— 直接传会编译不过(实测 E0521)。
        // 走 channel 后线程只发一个 f32,回调在主线程调用,借用关系天然成立。
        let (tx, rx) = std::sync::mpsc::channel::<f32>();

        // 独自占一个线程读 stderr:一边解析进度一边攒错误信息。
        // 不这样做的话,stderr 管道写满后子进程会阻塞 —— 那是很难查的死锁。
        let stderr = child.stderr.take().expect("已设置 piped stderr");
        let reader = std::thread::spawn(move || {
            let mut collected = String::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(p) = parse_progress_percent(&line) {
                    // 接收端已关闭时忽略错误 —— 说明调用方不再关心进度
                    let _ = tx.send(p);
                }
                collected.push_str(&line);
                collected.push('\n');
            }
            collected
        });

        // stdout 也读掉,避免同样的管道阻塞(whisper-cli 的内容都在文件里,
        // stdout 只有零散日志,直接丢弃)
        if let Some(out) = child.stdout.take() {
            std::thread::spawn(move || {
                for _ in BufReader::new(out).lines().map_while(Result::ok) {}
            });
        }

        // 等子进程结束,同时把攒下的进度转出去。
        // 200ms 一轮:对秒级刷新的进度条足够,也不会空转烧 CPU。
        let status = loop {
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(p) => on_progress(p),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    // 读线程结束 = stderr 已关闭,子进程也该退出了
                    break child.wait().context("等待 whisper-cli 结束失败")?;
                }
            }
            if let Some(s) = child.try_wait().context("查询 whisper-cli 状态失败")? {
                break s;
            }
        };

        // 进程结束后把 channel 里剩下的进度全部取出来,避免丢掉最后几个百分比
        while let Ok(p) = rx.try_recv() {
            on_progress(p);
        }
        let stderr_text = reader.join().unwrap_or_default();

        if !status.success() {
            let tail: String = stderr_text
                .lines()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .join("\n");
            return Err(anyhow!(
                "whisper-cli 退出码 {:?}:\n{}",
                status.code(),
                tail
            ));
        }

        let json_path = out_prefix.with_extension("json");
        let text = std::fs::read_to_string(&json_path).with_context(|| {
            format!("读取 whisper 输出失败: {}", json_path.display())
        })?;
        let parsed: WhisperJson = serde_json::from_str(&text)
            .with_context(|| format!("解析 whisper JSON 失败: {}", json_path.display()))?;
        Ok(parsed)
    }
}

/// 从 whisper-cli 的一行 stderr 里解析进度百分比。
///
/// 实测格式(v1.8.x,`-pp` 打开时):
///
/// ```text
/// whisper_print_progress_callback: progress =  26%
/// ```
///
/// 宽松匹配"含 progress 且以 % 结尾",不硬绑前缀 —— 版本间措辞可能变。
/// 解析不出来就返回 None(不报错)。
pub fn parse_progress_percent(line: &str) -> Option<f32> {
    if !line.contains("progress") {
        return None;
    }
    let pct_pos = line.rfind('%')?;
    let before = &line[..pct_pos];
    // 从 % 往前取连续的数字
    let digits: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if digits.is_empty() {
        return None;
    }
    let v: f32 = digits.parse().ok()?;
    Some((v / 100.0).clamp(0.0, 1.0))
}

impl Transcriber for WhisperCppSidecar {
    fn name(&self) -> &str {
        "whisper.cpp"
    }

    fn transcribe(&self, audio: &PcmAudio, opts: &AsrOpts) -> Result<Transcript> {
        // 无进度的调用:传一个空回调
        self.transcribe_with_progress(audio, opts, &|_| {})
    }

    /// 带进度的转写。
    ///
    /// 进度按**音频时长**加权聚合,而不是按块数:
    ///
    /// ```text
    /// 整体进度 = (已完成块的时长 + 当前块内进度 × 当前块时长) / 总时长
    /// ```
    ///
    /// 按块数算会在块时长不均时跳变 —— `plan_chunks` 把块边界对齐到静音处,
    /// 所以块长度本来就不齐。
    fn transcribe_with_progress(
        &self,
        audio: &PcmAudio,
        opts: &AsrOpts,
        on_progress: ProgressFn<'_>,
    ) -> Result<Transcript> {
        if audio.samples.is_empty() {
            return Err(anyhow!("音频为空"));
        }
        if !opts.model_path.is_file() {
            return Err(anyhow!(
                "模型文件不存在: {}\n请先下载模型(可用镜像 hf-mirror.com)。",
                opts.model_path.display()
            ));
        }

        let bin = self.resolve_bin(opts.backend)?;
        let tmp = tempfile::tempdir().context("创建临时目录失败")?;

        let chunks = audio::plan_chunks(audio, opts.chunk_target_ms);
        let total = audio.duration_ms();
        let mut all: Vec<Segment> = Vec::new();
        let mut language: Option<String> = None;

        // 已完成块的累计时长,用于算整体进度
        let mut done_ms: u64 = 0;

        for chunk in &chunks {
            let chunk_ms = chunk.end_ms.saturating_sub(chunk.start_ms);
            let seg = |p: f32| {
                let overall = if total > 0 {
                    (done_ms as f32 + p * chunk_ms as f32) / total as f32
                } else {
                    0.0
                };
                on_progress(overall.clamp(0.0, 1.0));
            };
            let part = self.transcribe_chunk(&bin, audio, chunk, opts, tmp.path(), &seg)?;
            if language.is_none() {
                language = part.0.clone();
            }
            all.extend(part.1);
            done_ms += chunk_ms;
        }

        // 收尾:确保最后一次上报是 1.0(块时长的舍入可能让它差一点点)
        on_progress(1.0);

        all.sort_by_key(|s| s.start_ms);
        let raw_text: String = all.iter().map(|s| s.text.as_str()).collect();

        Ok(Transcript {
            engine: "whisper.cpp".into(),
            model: opts
                .model_path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".into()),
            backend: opts.backend,
            backend_diarize: None,
            language,
            segments: all,
            raw_text,
            duration_ms: total,
            diarize: None,
        })
    }
}

impl WhisperCppSidecar {
    /// 转写单个块。切块偏移会加回时间戳,使结果落在全局时间轴上。
    fn transcribe_chunk(
        &self,
        bin: &Path,
        full: &PcmAudio,
        chunk: &Chunk,
        opts: &AsrOpts,
        tmpdir: &Path,
        on_progress: ProgressFn<'_>,
    ) -> Result<(Option<String>, Vec<Segment>)> {
        let slice = full.slice_ms(chunk.start_ms, chunk.end_ms);
        let wav = tmpdir.join(format!("chunk{:04}.wav", chunk.index));
        audio::encode_wav_16(&wav, &slice)?;

        let prefix = tmpdir.join(format!("out{:04}", chunk.index));
        let json = self.run_one(bin, &wav, &prefix, opts, on_progress)?;

        let segs: Vec<Segment> = json
            .transcription
            .iter()
            .filter_map(|s| {
                let text = s.text.trim().to_string();
                // 空段(纯静音/纯噪声)直接丢弃,避免污染转写
                if text.is_empty() {
                    return None;
                }
                Some(Segment {
                    start_ms: chunk.start_ms + s.offsets.from,
                    end_ms: chunk.start_ms + s.offsets.to.max(s.offsets.from),
                    text,
                    speaker_id: None,
                    overlapped: false,
                })
            })
            .collect();

        Ok((json.result.language, segs))
    }
}

// ---------------------------------------------------------------------------
// 假引擎(测试用,不依赖模型与二进制)
// ---------------------------------------------------------------------------

/// 确定性假引擎 —— 让端到端测试不依赖几 GB 的模型。
///
/// 它按输入音频时长生成固定段落,用来验证**管线连通性**:
/// 切块、时间轴拼接、缓存、说话人标注、输出格式。
#[derive(Debug, Default)]
pub struct MockTranscriber {
    /// 每段固定时长(毫秒)
    pub segment_ms: u64,
    /// 是否在文本里带上块序号,便于断言拼接正确
    pub label: bool,
}

impl MockTranscriber {
    pub fn new() -> Self {
        Self {
            segment_ms: 1000,
            label: true,
        }
    }
}

impl Transcriber for MockTranscriber {
    fn name(&self) -> &str {
        "mock"
    }

    fn transcribe(&self, audio: &PcmAudio, opts: &AsrOpts) -> Result<Transcript> {
        let total = audio.duration_ms();
        let step = self.segment_ms.max(1);
        let mut segments = Vec::new();
        let mut t = 0u64;
        let mut i = 0usize;
        while t < total {
            let end = (t + step).min(total);
            let text = if self.label {
                format!("<seg{i} {t}-{end}>")
            } else {
                "占位文本".to_string()
            };
            segments.push(Segment::new(t, end, text));
            t = end;
            i += 1;
        }
        let raw_text: String = segments.iter().map(|s| s.text.as_str()).collect();
        Ok(Transcript {
            engine: self.name().into(),
            model: "mock".into(),
            backend: opts.backend,
            backend_diarize: None,
            language: Some("zh".into()),
            segments,
            raw_text,
            duration_ms: total,
            diarize: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- 进度解析 ----------------------------------------------------------

    #[test]
    fn parses_real_whisper_progress_lines() {
        // ★ 这些行是从 whisper-cli v1.8.x 的**实际输出**里抄下来的
        //   (binaries/cuda/whisper-cli.exe -pp,137 秒音频)
        let cases = [
            ("whisper_print_progress_callback: progress =  26%", 0.26),
            ("whisper_print_progress_callback: progress =  55%", 0.55),
            ("whisper_print_progress_callback: progress =  83%", 0.83),
            ("whisper_print_progress_callback: progress = 100%", 1.0),
            ("whisper_print_progress_callback: progress =   0%", 0.0),
        ];
        for (line, want) in cases {
            let got = parse_progress_percent(line)
                .unwrap_or_else(|| panic!("应能解析: {line}"));
            assert!((got - want).abs() < 1e-6, "{line} → {got},期望 {want}");
        }
    }

    #[test]
    fn progress_parser_ignores_other_lines() {
        // whisper-cli 启动时会打一堆无关日志,不能误判成进度
        for line in [
            "ggml_cuda_init: found 1 CUDA devices (Total VRAM: 8187 MiB):",
            "  Device 0: NVIDIA GeForce RTX 4060 Laptop GPU, compute capability 8.9",
            "read_audio_data: reading audio data from 'x.wav' ...",
            "whisper_init_from_file_with_params_no_state: loading model",
            "",
            "progress without percent",
            "100%",
        ] {
            assert!(
                parse_progress_percent(line).is_none(),
                "不该解析出进度:{line:?}"
            );
        }
    }

    #[test]
    fn progress_parser_handles_variant_wording() {
        // 不硬绑前缀 —— 版本间措辞可能变,只要含 progress 且以 % 结尾就认
        assert_eq!(parse_progress_percent("progress = 42%"), Some(0.42));
        assert_eq!(parse_progress_percent("some progress 7%"), Some(0.07));
    }

    #[test]
    fn progress_parser_clamps_out_of_range() {
        // 防御:异常输入不该让进度条跑出 0~1
        assert_eq!(parse_progress_percent("progress = 150%"), Some(1.0));
    }

    fn tone(ms: u64) -> PcmAudio {
        let n = (16_000u64 * ms / 1000) as usize;
        PcmAudio {
            samples: vec![0.1; n],
            sample_rate: 16_000,
            channels: 1,
        }
    }

    #[test]
    fn parse_real_whisper_json_shape() {
        // 实测于 whisper.cpp v1.8.x 的真实输出结构
        let json = r#"{
            "systeminfo": "WHISPER : CPU : AVX = 1",
            "model": { "type": "tiny", "multilingual": true },
            "params": { "model": "ggml-tiny.bin", "language": "zh" },
            "result": { "language": "zh" },
            "transcription": [
                {
                    "timestamps": { "from": "00:00:00,000", "to": "00:00:02,000" },
                    "offsets": { "from": 0, "to": 2000 },
                    "text": " 今天讲神经网络"
                },
                {
                    "timestamps": { "from": "00:00:02,000", "to": "00:00:05,500" },
                    "offsets": { "from": 2000, "to": 5500 },
                    "text": " 首先看这个公式"
                }
            ]
        }"#;
        let p: WhisperJson = serde_json::from_str(json).unwrap();
        assert_eq!(p.result.language.as_deref(), Some("zh"));
        assert_eq!(p.transcription.len(), 2);
        assert_eq!(p.transcription[0].offsets.from, 0);
        assert_eq!(p.transcription[1].offsets.to, 5500);
        assert_eq!(p.transcription[0].text.trim(), "今天讲神经网络");
    }

    #[test]
    fn parse_tolerates_missing_fields() {
        let json = r#"{ "transcription": [ { "text": "x" } ] }"#;
        let p: WhisperJson = serde_json::from_str(json).unwrap();
        assert_eq!(p.transcription.len(), 1);
        assert_eq!(p.transcription[0].offsets.from, 0);
        assert!(p.result.language.is_none());
    }

    #[test]
    fn parse_empty_transcription() {
        let json = r#"{ "result": { "language": "zh" }, "transcription": [] }"#;
        let p: WhisperJson = serde_json::from_str(json).unwrap();
        assert!(p.transcription.is_empty());
    }

    #[test]
    fn initial_prompt_joins_and_truncates() {
        let mut o = AsrOpts::default();
        // 简体提示始终在,所以默认就有 prompt
        assert!(o.initial_prompt().is_some());
        o.hotwords = vec!["反向传播".into(), "梯度下降".into()];
        let p = o.initial_prompt().unwrap();
        assert!(p.contains("反向传播") && p.contains("梯度下降"), "{p}");

        o.hotwords = (0..500).map(|i| format!("词{i}")).collect();
        let p = o.initial_prompt().unwrap();
        assert!(p.chars().count() <= 200, "prompt 必须截断,否则超出上下文");
    }

    #[test]
    fn simplified_hint_is_always_injected() {
        // ★ 关键回归测试:简体提示**不按语言判断**。
        //
        //   实测 Whisyper 在中文音频上会稳定输出繁体("同學們"/"神經網絡"),
        //   而语言检测 ≠ 内容语言 —— 中英混合的课会被判成 en,
        //   但中文部分照样要出简体。按 language.starts_with("zh") 判断会让
        //   「自动检测」这个最适合混合内容的选项拿不到保护。
        for lang in [None, Some("zh"), Some("zh-CN"), Some("en"), Some("ja"), Some("auto")] {
            let mut o = AsrOpts::default();
            o.language = lang.map(|s| s.to_string());
            let p = o
                .initial_prompt()
                .unwrap_or_else(|| panic!("语言 {lang:?} 也必须带上简体提示"));
            assert!(p.contains("简体中文"), "语言 {lang:?} 的 prompt 缺少简体提示: {p}");
        }
    }

    #[test]
    fn hotwords_are_appended_after_hint() {
        let mut o = AsrOpts::default();
        o.hotwords = vec!["反向传播".into()];
        let p = o.initial_prompt().unwrap();
        assert!(p.contains("简体中文") && p.contains("反向传播"), "{p}");
        // 提示在前,热词在后(Whisper 的 initial_prompt 是前缀)
        let hint_at = p.find("简体中文").unwrap();
        let term_at = p.find("反向传播").unwrap();
        assert!(hint_at < term_at, "{p}");
    }

    #[test]
    fn hotwords_version_changes_with_content() {
        let mut a = AsrOpts::default();
        let v0 = a.hotwords_version();
        a.hotwords = vec!["甲".into()];
        let v1 = a.hotwords_version();
        a.hotwords = vec!["乙".into()];
        let v2 = a.hotwords_version();
        assert_eq!(v0, 0);
        assert_ne!(v1, v2, "不同热词表必须产生不同版本号(进缓存键)");
    }

    #[test]
    fn threads_auto_uses_cores() {
        let mut o = AsrOpts::default();
        assert!(o.effective_threads() >= 1);
        o.threads = 4;
        assert_eq!(o.effective_threads(), 4);
    }

    #[test]
    fn build_args_sets_language_explicitly() {
        // ⚠️ whisper-cli 的 -l 默认是 "en",不显式传会导致中文音频走英文解码
        let s = WhisperCppSidecar::new(SidecarLocator::new("x"));
        let mut o = AsrOpts::default();
        o.model_path = PathBuf::from("m.bin");
        let args = s.build_args(Path::new("a.wav"), Path::new("o"), &o);
        let li = args.iter().position(|a| a == "-l").expect("必须传 -l");
        assert_eq!(args[li + 1], "auto");

        o.language = Some("zh".into());
        let args = s.build_args(Path::new("a.wav"), Path::new("o"), &o);
        let li = args.iter().position(|a| a == "-l").unwrap();
        assert_eq!(args[li + 1], "zh");
    }

    #[test]
    fn build_args_never_sets_max_len() {
        // ★ -ml(--max-len)会触发字级时间戳,那在 CJK 上工作得很差
        let s = WhisperCppSidecar::new(SidecarLocator::new("x"));
        let o = AsrOpts::default();
        let args = s.build_args(Path::new("a.wav"), Path::new("o"), &o);
        assert!(
            !args.iter().any(|a| a == "-ml" || a == "--max-len"),
            "绝不能传 --max-len:CJK 字级时间戳不可靠"
        );
        assert!(args.iter().any(|a| a == "-oj"), "必须请求 JSON 输出");
    }

    #[test]
    fn build_args_always_includes_prompt() {
        let s = WhisperCppSidecar::new(SidecarLocator::new("x"));
        let mut o = AsrOpts::default();

        // 简体提示始终注入,所以即使没有热词也应有 --prompt
        let args = s.build_args(Path::new("a.wav"), Path::new("o"), &o);
        let pi = args
            .iter()
            .position(|a| a == "--prompt")
            .expect("简体提示应始终注入");
        assert!(args[pi + 1].contains("简体中文"), "{}", args[pi + 1]);

        // 加热词后,提示里应同时包含两者
        o.hotwords = vec!["术语".into()];
        let args = s.build_args(Path::new("a.wav"), Path::new("o"), &o);
        let pi = args.iter().position(|a| a == "--prompt").unwrap();
        assert!(
            args[pi + 1].contains("简体中文") && args[pi + 1].contains("术语"),
            "{}",
            args[pi + 1]
        );
    }

    #[test]
    fn missing_binary_gives_actionable_error() {
        let s = WhisperCppSidecar::new(SidecarLocator::new("definitely/not/here"));
        let err = s.resolve_bin(Backend::Cuda).unwrap_err().to_string();
        assert!(err.contains("cuda"), "错误应指出期望路径: {err}");
    }

    #[test]
    fn mock_transcriber_covers_full_duration() {
        let m = MockTranscriber::new();
        let t = m.transcribe(&tone(3500), &AsrOpts::default()).unwrap();
        assert_eq!(t.duration_ms, 3500);
        assert_eq!(t.segments.first().unwrap().start_ms, 0);
        assert_eq!(t.segments.last().unwrap().end_ms, 3500);
        // 段落首尾相接
        for w in t.segments.windows(2) {
            assert_eq!(w[0].end_ms, w[1].start_ms);
        }
    }

    #[test]
    fn mock_transcriber_handles_zero_duration() {
        let m = MockTranscriber::new();
        let a = PcmAudio {
            samples: vec![],
            sample_rate: 16_000,
            channels: 1,
        };
        let t = m.transcribe(&a, &AsrOpts::default()).unwrap();
        assert!(t.segments.is_empty());
    }

    #[test]
    fn real_transcriber_rejects_empty_audio() {
        let s = WhisperCppSidecar::new(SidecarLocator::new("x"));
        let a = PcmAudio {
            samples: vec![],
            sample_rate: 16_000,
            channels: 1,
        };
        assert!(s.transcribe(&a, &AsrOpts::default()).is_err());
    }

    #[test]
    fn real_transcriber_rejects_missing_model() {
        let s = WhisperCppSidecar::new(SidecarLocator::new("x"));
        let mut o = AsrOpts::default();
        o.model_path = PathBuf::from("no/such/model.bin");
        let err = s.transcribe(&tone(100), &o).unwrap_err().to_string();
        assert!(err.contains("模型文件不存在"), "{err}");
    }
}
