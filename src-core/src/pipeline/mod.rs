//! 管线编排:解码 → 转写 → 说话人区分 → 场景判断 → 纪要。
//!
//! 见技术方案 §4.7 与 §12。设计要点:
//!
//! - **所有耗时接口都接受 `ProgressSink`**,长任务必须能报进度、能被取消。
//! - **分阶段出结果** —— 转写完成就立刻可读,diarization 在后台继续。
//! - **每个阶段独立降级** —— 转写可能用 CUDA,diarization 走 CPU,两者互不影响。
//! - **缓存优先** —— 同一音频只转写一次;改总结模板重跑是秒级的。

pub mod view;

use crate::asr::{AsrOpts, Transcriber};
use crate::audio::{self, PcmAudio};
use crate::diarize;
use crate::hardware::{self, HardwareProfile, SidecarLocator};
use crate::llm::Summarizer;
use crate::store::cache::{self, CacheKeyInputs, DiarizeCacheKey, TranscriptCacheKey};
use crate::store::files::FileStore;
use crate::store::Db;
use crate::types::{
    content_hash_of, AudioRef, AudioSourceKind, Backend, BackendPref, DiarizeInfo, DiarizeMode,
    MindMap, ModelTier, Scene, SceneVerdict, SpeakerLabels, Summary, TokenUsage, Transcript,
};
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// 进度与取消
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Probe,
    Decode,
    Transcribe,
    Diarize,
    /// 转写纠错(用 LLM 修同音字与术语)。见 `llm::correct`。
    Correct,
    Identify,
    SceneDetect,
    Summarize,
    Persist,
}

impl Stage {
    pub fn label(&self) -> &'static str {
        match self {
            Stage::Probe => "硬件探测",
            Stage::Decode => "解码音频",
            Stage::Transcribe => "转写",
            Stage::Diarize => "说话人区分",
            Stage::Correct => "转写纠错",
            Stage::Identify => "声纹辨认",
            Stage::SceneDetect => "场景判断",
            Stage::Summarize => "生成纪要",
            Stage::Persist => "保存结果",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Progress {
    /// 开始一个阶段
    StageStart(Stage, u32),
    /// 阶段内进度
    StagePct(Stage, f32),
    /// 转写:已处理音频时长 —— ★ 比百分比更能让用户估算"还要多久"
    Transcribe { audio_ms_done: u64, audio_ms_total: u64 },
    /// 缓存命中,跳过耗时阶段
    CacheHit(Stage),
    /// 提示信息(会显示给用户)
    Note(String),
}

/// 进度回调 + 取消标志。
#[derive(Clone)]
pub struct ProgressSink {
    inner: Option<Arc<ProgressInner>>,
    cancel: Arc<AtomicBool>,
}

struct ProgressInner {
    cb: Option<Box<dyn Fn(Progress) + Send + Sync>>,
}

impl Default for ProgressInner {
    fn default() -> Self {
        Self { cb: None }
    }
}

impl ProgressSink {
    pub fn new<F>(cb: F) -> Self
    where
        F: Fn(Progress) + Send + Sync + 'static,
    {
        Self {
            inner: Some(Arc::new(ProgressInner {
                cb: Some(Box::new(cb)),
            })),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 什么都不做的 sink(测试与静默模式)。
    pub fn silent() -> Self {
        Self {
            inner: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn report(&self, p: Progress) {
        if let Some(inner) = &self.inner {
            if let Some(cb) = &inner.cb {
                cb(p);
            }
        }
    }

    pub fn note(&self, msg: impl Into<String>) {
        self.report(Progress::Note(msg.into()));
    }

    pub fn stage_start(&self, stage: Stage, total: u32) {
        self.report(Progress::StageStart(stage, total));
    }

    pub fn stage_pct(&self, stage: Stage, pct: f32) {
        self.report(Progress::StagePct(stage, pct.clamp(0.0, 1.0)));
    }

    pub fn cache_hit(&self, stage: Stage) {
        self.report(Progress::CacheHit(stage));
    }

    /// 转写阶段的音频级进度。
    ///
    /// 与 [`Self::stage_pct`] 的区别:那个是"阶段完成了百分之几"(只有 0/1),
    /// 这个是"**已经处理了多少音频**"。转写是唯一一个耗时长到需要细粒度
    /// 进度的阶段,所以要单独一条通道。
    ///
    /// `done_ms` / `total_ms` 都按**音频时长**算 —— 用户据此能估算剩余时间,
    /// 比一个百分比有用得多。
    pub fn transcribe_progress(&self, done_ms: u64, total_ms: u64) {
        self.report(Progress::Transcribe {
            audio_ms_done: done_ms.min(total_ms),
            audio_ms_total: total_ms,
        });
    }

    /// 取消句柄 —— 由调用方触发(可能来自另一个线程)。
    pub fn canceller(&self) -> Canceller {
        Canceller {
            flag: Arc::clone(&self.cancel),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// 在每个耗时步骤前调用,已取消则中断。
    ///
    /// ★ 中断是**协作式**的:已完成的块留在缓存里,重跑时从断点继续(见技术方案 §3.4)。
    pub fn check_cancelled(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(anyhow!("已取消"))
        } else {
            Ok(())
        }
    }
}

/// 取消句柄。可在另一个线程调用。
#[derive(Clone)]
pub struct Canceller {
    flag: Arc<AtomicBool>,
}

impl Canceller {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// 配置与结果
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct PipelineConfig {
    /// 数据根目录
    pub data_dir: PathBuf,
    /// sidecar 二进制根目录(其下按后端分子目录)
    pub binaries_dir: PathBuf,
    /// 模型目录
    pub models_dir: PathBuf,
    /// ffmpeg 路径(可选,找不到会去 PATH 与 binaries/ffmpeg 找)
    pub ffmpeg: Option<PathBuf>,

    pub backend_pref: BackendPref,
    /// 覆盖自动推荐的模型档位
    pub model_override: Option<ModelTier>,
    pub language: Option<String>,
    pub hotwords: Vec<String>,
    pub chunk_target_ms: u64,

    pub diarize: DiarizeMode,
    pub enable_diarize: bool,

    /// 是否调用 LLM 生成纪要
    pub enable_summary: bool,
    /// 是否做术语校正(建议开,成本极低但收益明显)
    pub enable_term_correction: bool,
}

impl PipelineConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        Self {
            binaries_dir: data_dir.join("binaries"),
            models_dir: data_dir.join("models"),
            data_dir,
            ffmpeg: None,
            backend_pref: BackendPref::Auto,
            model_override: None,
            language: None,
            hotwords: vec![],
            chunk_target_ms: 5 * 60 * 1000,
            diarize: DiarizeMode::Auto,
            enable_diarize: true,
            enable_summary: true,
            enable_term_correction: true,
        }
    }

    pub fn with_binaries(mut self, dir: impl Into<PathBuf>) -> Self {
        self.binaries_dir = dir.into();
        self
    }

    pub fn with_models(mut self, dir: impl Into<PathBuf>) -> Self {
        self.models_dir = dir.into();
        self
    }

    pub fn with_backend(mut self, pref: BackendPref) -> Self {
        self.backend_pref = pref;
        self
    }

    pub fn with_model(mut self, tier: ModelTier) -> Self {
        self.model_override = Some(tier);
        self
    }

    pub fn with_language(mut self, lang: Option<String>) -> Self {
        self.language = lang;
        self
    }

    pub fn with_diarize(mut self, mode: DiarizeMode) -> Self {
        self.diarize = mode;
        self.enable_diarize = !matches!(mode, DiarizeMode::Fixed(0));
        self
    }

    pub fn model_path(&self, tier: ModelTier) -> PathBuf {
        self.models_dir.join(tier.file_name())
    }
}

/// 管线产出。
#[derive(Clone, Debug)]
pub struct PipelineOutcome {
    pub session_id: String,
    pub transcript: Transcript,
    pub labels: SpeakerLabels,
    pub scene: Option<SceneVerdict>,
    /// 详细总结(完整结构)
    pub summary: Option<Summary>,
    /// 简略总结(一页速览)。与详细版**目标不同**,不是它的压缩。
    pub summary_brief: Option<Summary>,
    /// 思维导图(Mermaid)
    pub mindmap: Option<MindMap>,
    pub usage: TokenUsage,
    pub hardware: Option<HardwareProfile>,
    /// 各阶段是否命中缓存(便于验证缓存设计真的生效)
    pub transcript_from_cache: bool,
    pub embeddings_from_cache: bool,
    /// 纠错情况的说明(未启用或未配置 LLM 时为 None)。
    ///
    /// 界面上要显示它 —— "纠错改了 37 段"和"纠错被跳过了"对用户
    /// 是完全不同的信息,不能都不出声。
    pub correction_note: Option<String>,
    /// 本次生成/复用的工程目录(None = 未启用工程制)
    pub project_dir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// 管线
// ---------------------------------------------------------------------------

pub struct Pipeline<'a> {
    cfg: PipelineConfig,
    db: Db,
    files: FileStore,
    transcriber: &'a dyn Transcriber,
    summarizer: Option<&'a dyn Summarizer>,
}

impl<'a> Pipeline<'a> {
    pub fn new(
        cfg: PipelineConfig,
        transcriber: &'a dyn Transcriber,
        summarizer: Option<&'a dyn Summarizer>,
    ) -> Result<Self> {
        let db = Db::open(&cfg.data_dir.join("cache.db"))?;
        let files = FileStore::new(cfg.data_dir.join("store"));
        files.ensure_dirs()?;
        Ok(Self {
            cfg,
            db,
            files,
            transcriber,
            summarizer,
        })
    }

    pub fn files(&self) -> &FileStore {
        &self.files
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// 硬件探测:按优先级找可用后端 + 推荐模型。
    pub fn probe_hardware(&self, progress: &ProgressSink) -> HardwareProfile {
        progress.stage_start(Stage::Probe, 1);
        let locator = SidecarLocator::new(&self.cfg.binaries_dir);
        let cores = hardware::cpu_cores();
        let vram = hardware::nvidia_vram_gb();

        let installed = locator.installed();
        let mut probes = Vec::new();
        for (backend, _path) in &installed {
            let avail = hardware::available(*backend);
            let device = match backend {
                Backend::Cuda => hardware::nvidia_smi_devices().and_then(|d| d.into_iter().next()),
                _ => None,
            };
            probes.push(hardware::ProbeResult {
                backend: *backend,
                available: avail,
                // 真正的自检(试跑微型转写)在第一次实际转写时完成 —— 见技术方案 §3.2。
                // 这里先记录可用性;实测 rtf 会在转写后回填。
                ok: avail,
                elapsed_ms: 0,
                rtf: 0.0,
                device_name: device,
                error: if avail {
                    None
                } else {
                    Some(hardware::explain_unavailable(*backend, &locator))
                },
            });
        }

        let requested = match self.cfg.backend_pref {
            BackendPref::Force(b) => Some(b),
            BackendPref::Auto => None,
        };

        let selected = match self.cfg.backend_pref {
            BackendPref::Force(b) => b,
            BackendPref::Auto => hardware::select_backend(BackendPref::Auto, &locator)
                .unwrap_or(Backend::Cpu),
        };

        // 后端可用但没装二进制时,给出明确提示而不是静默退回 CPU
        if let BackendPref::Force(b) = self.cfg.backend_pref {
            if locator.whisper_cli(b).is_none() {
                progress.note(format!(
                    "⚠ 指定了 {} 后端但未找到对应程序:{}",
                    b.label(),
                    hardware::explain_unavailable(b, &locator)
                ));
            }
        }

        let device_name = probes
            .iter()
            .find(|p| p.backend == selected)
            .and_then(|p| p.device_name.clone());

        let model_recommended = self
            .cfg
            .model_override
            .unwrap_or_else(|| hardware::recommend_model(selected, vram, cores));

        let profile = HardwareProfile {
            backend_selected: selected,
            backend_requested: requested,
            device_name,
            vram_gb: vram,
            cpu_cores: cores,
            rtf_measured: 0.0,
            model_recommended,
            probed_at: crate::store::db::now_ms(),
            probes,
        };

        let _ = self.db.save_hardware_profile(&profile);
        progress.stage_pct(Stage::Probe, 1.0);
        profile
    }

    /// 跑完整条管线。
    pub fn run(&self, input: &AudioRef, progress: &ProgressSink) -> Result<PipelineOutcome> {
        let hw = self.probe_hardware(progress);
        progress.note(format!(
            "硬件:{} · 模型:{}",
            hw.summary_line(),
            hw.model_recommended.label()
        ));

        // ---------- 解码 ----------
        progress.stage_start(Stage::Decode, 1);
        progress.check_cancelled()?;
        let pcm = audio::decode_audio(
            &input.path,
            self.cfg.ffmpeg.as_deref(),
            Some(&self.cfg.binaries_dir),
        )
        .with_context(|| format!("解码失败: {}", input.path.display()))?;
        let duration_ms = pcm.duration_ms();
        progress.note(format!(
            "音频时长 {}",
            hardware::humanize_duration(std::time::Duration::from_millis(duration_ms))
        ));

        if hardware::should_warn_slow(hw.backend_selected, duration_ms) {
            let est = hw.estimate_duration(duration_ms);
            progress.note(format!(
                "⚠ 未检测到可用 GPU,预计需要 {}。可随时取消,已完成的进度会保留。",
                hardware::humanize_duration(est)
            ));
        }
        progress.stage_pct(Stage::Decode, 1.0);

        // ---------- 内容哈希与会话 ----------
        let session_id = match &input.content_hash {
            Some(h) => h.clone(),
            None => content_hash_of(&input.path).context("计算音频哈希失败")?,
        };

        // ---------- 转写(带缓存) ----------
        let model_tier = hw.model_recommended;
        let model_path = self.cfg.model_path(model_tier);
        let asr_opts = AsrOpts {
            model_path: model_path.clone(),
            backend: hw.backend_selected,
            language: self.cfg.language.clone(),
            hotwords: self.cfg.hotwords.clone(),
            vad: true,
            threads: 0,
            chunk_target_ms: self.cfg.chunk_target_ms,
        };

        let cache_inputs = CacheKeyInputs {
            audio_hash: session_id.clone(),
            asr_model: model_tier.label().to_string(),
            backend: hw.backend_selected,
            language: asr_opts.language.clone(),
            diarize_enabled: self.cfg.enable_diarize,
            diarize_model: Some("sherpa-onnx".into()),
            num_speakers: self.cfg.diarize,
            vad: true,
            hotwords_version: asr_opts.hotwords_version(),
            chunk_index: None,
        };
        let tkey = TranscriptCacheKey::compute(&cache_inputs);

        progress.stage_start(Stage::Transcribe, 1);
        let (mut transcript, from_cache) = match cache::get_transcript(&self.db, &tkey)? {
            Some(json) => {
                progress.cache_hit(Stage::Transcribe);
                (serde_json::from_str::<Transcript>(&json)?, true)
            }
            None => {
                progress.check_cancelled()?;
                // ★ 把转写进度实时转出去。
                //
                // 没有这一步,进度条会在整个转写阶段停在 0% ——
                // CPU 上跑一节 45 分钟的课是几分钟的静止,用户无法区分
                // "在跑"和"卡死"。whisper-cli 的 -pp 本来就输出进度,
                // 这里只是把它接到界面上。
                let t = self.transcriber.transcribe_with_progress(
                    &pcm,
                    &asr_opts,
                    &|p: f32| {
                        progress.transcribe_progress(
                            (p * duration_ms as f32) as u64,
                            duration_ms,
                        );
                    },
                )?;
                cache::put_transcript(
                    &self.db,
                    &tkey,
                    &session_id,
                    None,
                    &serde_json::to_string(&t)?,
                )?;
                progress.note(format!(
                    "转写完成:{} 段",
                    t.segments.len()
                ));
                (t, false)
            }
        };
        progress.stage_pct(Stage::Transcribe, 1.0);

        // ---------- 说话人区分 ----------
        let mut embeddings_from_cache = false;
        if self.cfg.enable_diarize && !transcript.segments.is_empty() {
            self.diarize_stage(
                &session_id,
                &pcm,
                &mut transcript,
                &hw,
                progress,
                &mut embeddings_from_cache,
            )?;
        }

        // ---------- 转写纠错 ----------
        //
        // 放在**说话人区分之后、生成纪要之前**,因为:
        //
        // - 声纹辨认依赖音频,与文字无关,先做后做都一样
        // - 纪要要吃纠错后的文本,否则总结里那些"转写中 X 应为 Y"的
        //   括号注释就成了打补丁 —— 而用户要的是**转写本身**是对的
        //
        // ⚠️ 这一步会**直接改写 transcript 的文本**(用户明确选了这种形式),
        //    所以 correct.rs 里有一整套"宁可漏改不可错改"的校验。
        //    时间戳不受影响 —— 只替换 text,不动 start_ms/end_ms。
        let mut correction_note: Option<String> = None;
        if self.cfg.enable_term_correction && !transcript.segments.is_empty() {
            if let Some(sum) = self.summarizer {
                progress.stage_start(Stage::Correct, 1);
                progress.check_cancelled()?;
                let (stats, warns) =
                    sum.correct_blocking(&mut transcript.segments, &self.cfg.hotwords);
                crate::llm::correct::rebuild_raw_text(&mut transcript);
                for w in warns.iter().take(3) {
                    progress.note(format!("⚠ {w}"));
                }
                let line = stats.summary();
                progress.note(format!("纠错:{line}"));
                correction_note = Some(line);
                progress.stage_pct(Stage::Correct, 1.0);
            } else {
                // 没配 LLM 就跳过,但要说清楚 —— 否则用户会以为纠错跑了
                progress.note("跳过纠错(未配置 LLM)");
            }
        }

        // ---------- 标签 ----------
        let mut labels = self
            .files
            .read_labels(&session_id)?
            .unwrap_or_else(|| SpeakerLabels::new(session_id.clone()));
        let ids: Vec<u32> = transcript
            .segments
            .iter()
            .filter_map(|s| s.speaker_id)
            .collect();
        labels.ensure(ids);

        // ---------- 场景判断 + 纪要 ----------
        let mut scene_verdict = None;
        let mut summary = None;
        let mut summary_brief = None;
        let mut mindmap = None;
        let mut usage = TokenUsage::default();

        if self.cfg.enable_summary {
            if let Some(sum) = self.summarizer {
                progress.stage_start(Stage::SceneDetect, 1);
                progress.check_cancelled()?;
                match sum.detect_scene(&transcript) {
                    Ok(v) => {
                        progress.note(format!(
                            "场景:{} (置信度 {:.0}%){}",
                            v.scene.label(),
                            v.confidence * 100.0,
                            if v.is_low_confidence() {
                                " — 判断不太确定,可手动切换模板"
                            } else {
                                ""
                            }
                        ));
                        scene_verdict = Some(v);
                    }
                    Err(e) => {
                        progress.note(format!("场景判断失败,按默认模板处理: {e}"));
                        scene_verdict = Some(SceneVerdict {
                            scene: Scene::Other,
                            confidence: 0.0,
                            evidence: format!("判断失败: {e}"),
                            suggested_focus: vec![],
                        });
                    }
                }
                progress.stage_pct(Stage::SceneDetect, 1.0);

                let scene = scene_verdict.as_ref().map(|v| v.scene).unwrap_or(Scene::Other);

                // ① 详细总结(完整结构)
                progress.stage_start(Stage::Summarize, 3);
                progress.check_cancelled()?;
                match sum.summarize(&transcript, scene, &labels, labels.labels_version) {
                    Ok(s) => {
                        usage.merge(&s.usage);
                        progress.note(format!(
                            "详细总结完成({} tokens,缓存命中 {})",
                            s.usage.input + s.usage.output,
                            s.usage.cached_input
                        ));
                        summary = Some(s);
                    }
                    Err(e) => {
                        progress.note(format!("详细总结失败: {e}"));
                    }
                }
                progress.stage_pct(Stage::Summarize, 1.0 / 3.0);

                // ② 简略总结(一页速览)
                //
                // 与详细版**不是同一份东西的压缩**,而是换了目标 ——
                // 所以这里单独调用,不复用详细版的结果。
                progress.check_cancelled()?;
                match sum.summarize_brief(&transcript, scene, &labels, labels.labels_version) {
                    Ok(s) if !s.content_md.trim().is_empty() => {
                        usage.merge(&s.usage);
                        progress.note(format!(
                            "简略总结完成({} tokens)",
                            s.usage.input + s.usage.output
                        ));
                        summary_brief = Some(s);
                    }
                    Ok(_) => {
                        progress.note("简略总结为空,已跳过");
                    }
                    Err(e) => {
                        progress.note(format!("简略总结失败: {e}"));
                    }
                }
                progress.stage_pct(Stage::Summarize, 2.0 / 3.0);

                // ③ 思维导图(Mermaid)
                progress.check_cancelled()?;
                match sum.summarize_mindmap(&transcript, scene) {
                    Ok(mm) if !mm.is_empty() => {
                        if mm.syntax_ok {
                            progress.note("思维导图已生成");
                        } else {
                            progress.note(format!(
                                "⚠ 思维导图语法自检未通过({}),已同时保留文本大纲作为备份",
                                mm.syntax_issues.join("、")
                            ));
                        }
                        mindmap = Some(mm);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        progress.note(format!("思维导图生成失败: {e}"));
                    }
                }
                progress.stage_pct(Stage::Summarize, 1.0);
            }
        }

        // ---------- 落盘 ----------
        //
        // 两条路都写:
        // - 哈希存储:内部去重与缓存,跨设备同步靠它
        // - 工程目录:面向用户,可打包、可阅读
        progress.stage_start(Stage::Persist, 2);
        self.files.write_transcript(&transcript, &session_id)?;
        self.files.write_labels(&labels)?;
        if let Some(s) = &summary {
            self.files.write_summary(s, &session_id)?;
        }
        progress.stage_pct(Stage::Persist, 0.5);

        // ---------- 工程目录 ----------
        let project_dir = match self.persist_project(
            &session_id,
            input,
            &transcript,
            &labels,
            &scene_verdict,
            &summary,
            &summary_brief,
            &mindmap,
            duration_ms,
            progress,
        ) {
            Ok(d) => d,
            Err(e) => {
                // 工程建立失败不该让整条管线失败 —— 转写与总结已经落盘了
                progress.note(format!("⚠ 工程目录创建失败(其他产物已保存): {e:#}"));
                None
            }
        };
        progress.stage_pct(Stage::Persist, 1.0);

        self.db.upsert_session(&crate::store::db::SessionRow {
            id: session_id.clone(),
            title: input
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string()),
            created_at: crate::store::db::now_ms(),
            duration_ms,
            audio_local_path: Some(input.path.to_string_lossy().to_string()),
            source_type: match input.source {
                AudioSourceKind::File => "file",
                AudioSourceKind::DualTrack => "dual_track",
                AudioSourceKind::Mic => "mic",
            }
            .into(),
            status: "done".into(),
            scene: scene_verdict.as_ref().map(|v| v.scene),
            scene_confidence: scene_verdict.as_ref().map(|v| v.confidence),
            last_modified_at: crate::store::db::now_ms(),
            origin_device: None,
        })?;
        progress.stage_pct(Stage::Persist, 1.0);

        Ok(PipelineOutcome {
            session_id,
            transcript,
            labels,
            scene: scene_verdict,
            summary,
            summary_brief,
            mindmap,
            usage,
            hardware: Some(hw),
            transcript_from_cache: from_cache,
            embeddings_from_cache,
            correction_note,
            project_dir,
        })
    }

    /// 建立(或复用)工程目录,并把所有产物写进去。
    ///
    /// 工程制是**面向用户的视图**,与哈希存储并存:
    /// - 哈希存储负责去重与缓存(同一音频不重复转写)
    /// - 工程目录负责"可打包、可阅读、可同步"
    ///
    /// 因为工程 `id` 就是音频内容哈希,**同一录音重复处理会复用同一个工程**,
    /// 不会每次生成一个新目录。
    #[allow(clippy::too_many_arguments)]
    fn persist_project(
        &self,
        session_id: &str,
        input: &AudioRef,
        transcript: &Transcript,
        labels: &SpeakerLabels,
        scene: &Option<SceneVerdict>,
        summary: &Option<Summary>,
        summary_brief: &Option<Summary>,
        mindmap: &Option<MindMap>,
        duration_ms: u64,
        progress: &ProgressSink,
    ) -> Result<Option<PathBuf>> {
        use crate::project::{self, ProjectStore, SummaryKind};

        // 标题优先用原文件名(用户认得),没有就用短的会话 ID
        let title = input
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("录音 {}", &session_id[..8.min(session_id.len())]));

        let store = ProjectStore::new(self.files.root());
        let mut p = store.create_or_open(session_id, &title)?;

        // 录音:拷进工程,让工程自包含(可以整个目录打包发给别人)
        match project::copy_audio(&p.dir, &input.path, session_id) {
            Ok(_) => {}
            Err(e) => progress.note(format!("⚠ 录音拷贝失败(工程仍会建立): {e}")),
        }

        project::write_transcript_artifacts(&p.dir, transcript, labels)?;
        if let Some(s) = summary {
            project::write_summary(&p.dir, SummaryKind::Detailed, s)?;
        }
        if let Some(s) = summary_brief {
            project::write_summary(&p.dir, SummaryKind::Brief, s)?;
        }
        if let Some(mm) = mindmap {
            project::write_mindmap(&p.dir, mm)?;
        }

        // 元数据
        p.meta.title = title;
        p.meta.duration_ms = duration_ms;
        p.meta.scene = scene.as_ref().map(|v| v.scene);
        p.meta.scene_confidence = scene.as_ref().map(|v| v.confidence);
        p.meta.source_path = Some(input.path.to_string_lossy().to_string());
        p.meta.asr_model = Some(transcript.model.clone());
        p.meta.asr_backend = Some(transcript.backend.as_str().to_string());
        p.meta.has_speakers = transcript.has_speakers();
        p.meta.speaker_count = transcript
            .diarize
            .as_ref()
            .map(|d| d.num_speakers_detected)
            .unwrap_or(0);
        p.refresh_artifacts()?;

        progress.note(format!("工程目录:{}", p.dir.display()));
        Ok(Some(p.dir))
    }

    /// 说话人区分。
    ///
    /// 调 sherpa-onnx 的离线说话人区分(分割 → 嵌入 → 聚类一体)。
    /// 结果**按「音频 + 模型」缓存**,所以改人数重跑时能跳过最贵的那一步。
    fn diarize_stage(
        &self,
        session_id: &str,
        pcm: &PcmAudio,
        transcript: &mut Transcript,
        _hw: &HardwareProfile,
        progress: &ProgressSink,
        from_cache: &mut bool,
    ) -> Result<()> {
        progress.stage_start(Stage::Diarize, 1);

        // 模型缺失时**不要失败整条管线** —— 降级为"无说话人信息"。
        // 这比"缺个模型就全废"好得多:用户仍然拿得到转写和纪要。
        if !crate::sherpa::models_present(&self.cfg.models_dir) {
            let missing = crate::sherpa::missing_models(&self.cfg.models_dir);
            progress.note(format!(
                "⚠ 说话人区分已跳过(声纹模型未就位):\n  {}\n  \
                 转写与纪要不受影响。下载模型后重新处理即可补做(转写会命中缓存)。",
                missing
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n  ")
            ));
            return Ok(());
        }

        let dia_backend = Backend::Cpu;
        let ekey = DiarizeCacheKey::compute(
            session_id,
            crate::sherpa::SEGMENTATION_MODEL,
            crate::sherpa::EMBEDDING_MODEL,
            dia_backend,
        );

        let intervals: Vec<crate::diarize::SpeakerInterval> =
            match cache::get_diarize(&self.db, &ekey)? {
                Some(json) => {
                    progress.cache_hit(Stage::Diarize);
                    *from_cache = true;
                    serde_json::from_str(&json).unwrap_or_default()
                }
                None => {
                    progress.check_cancelled()?;
                    let d = crate::sherpa::SherpaDiarizer::new(&self.cfg.models_dir)?;
                    // sherpa 要 16kHz 单声道 f32,这正是 to_asr_format 的产物
                    let samples = pcm.to_asr_format().samples;
                    // 进度回调要求 Send + 'static,所以把 sink 克隆进闭包
                    let sink = progress.clone();
                    let outcome = d.run(samples, self.cfg.diarize, move |p| {
                        sink.stage_pct(Stage::Diarize, p);
                    })?;
                    cache::put_diarize(
                        &self.db,
                        &ekey,
                        session_id,
                        &serde_json::to_string(&outcome.intervals)?,
                    )?;
                    outcome.intervals
                }
            };

        if intervals.is_empty() {
            progress.note("⚠ 没有说话人片段(音频可能全是静音或噪声)");
            return Ok(());
        }

        diarize::annotate_segments(&mut transcript.segments, &intervals);

        let stats = crate::sherpa::speaker_stats(&intervals);
        let num_speakers = stats.len() as u32;

        transcript.diarize = Some(DiarizeInfo {
            model: crate::sherpa::EMBEDDING_MODEL.to_string(),
            backend: dia_backend,
            mode: self.cfg.diarize,
            num_speakers_detected: num_speakers,
            speakers: stats,
        });

        // 重叠语音的数量是个有用的信号:它告诉用户这句可能是抢话
        let overlapped = transcript.segments.iter().filter(|s| s.overlapped).count();
        let mut msg = format!("检测到 {num_speakers} 位发言人");
        if overlapped > 0 {
            msg.push_str(&format!(",其中 {overlapped} 段为多人重叠(已标记)"));
        }
        progress.note(msg);
        progress.stage_pct(Stage::Diarize, 1.0);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asr::MockTranscriber;
    use crate::llm::MockSummarizer;
    use std::sync::Mutex;

    fn tone_pcm(ms: u64) -> PcmAudio {
        PcmAudio {
            samples: vec![0.05; (16_000u64 * ms / 1000) as usize],
            sample_rate: 16_000,
            channels: 1,
        }
    }

    fn write_wav(dir: &std::path::Path, ms: u64) -> PathBuf {
        let p = dir.join("input.wav");
        audio::encode_wav_16(&p, &tone_pcm(ms)).unwrap();
        p
    }

    struct Harness {
        _dir: tempfile::TempDir,
        cfg: PipelineConfig,
    }

    fn harness() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let cfg = PipelineConfig::new(dir.path().join("data"));
        Harness { _dir: dir, cfg }
    }

    #[test]
    fn progress_events_are_emitted_in_order() {
        let h = harness();
        let wav = write_wav(h._dir.path(), 3000);
        let seen: Arc<Mutex<Vec<Stage>>> = Arc::new(Mutex::new(vec![]));
        let s2 = Arc::clone(&seen);
        let sink = ProgressSink::new(move |p| {
            if let Progress::StageStart(st, _) = p {
                s2.lock().unwrap().push(st);
            }
        });

        let tc = MockTranscriber::new();
        let sum = MockSummarizer::new();
        let pipe = Pipeline::new(h.cfg.clone(), &tc, Some(&sum)).unwrap();
        let input = AudioRef {
            path: wav,
            duration_ms: None,
            content_hash: None,
            source: AudioSourceKind::File,
        };
        pipe.run(&input, &sink).unwrap();

        let stages = seen.lock().unwrap().clone();
        let idx = |s: Stage| stages.iter().position(|x| *x == s).unwrap();
        assert!(idx(Stage::Probe) < idx(Stage::Decode));
        assert!(idx(Stage::Decode) < idx(Stage::Transcribe));
        assert!(idx(Stage::Transcribe) < idx(Stage::SceneDetect));
        assert!(idx(Stage::SceneDetect) < idx(Stage::Summarize));
        assert!(idx(Stage::Summarize) < idx(Stage::Persist));
    }

    #[test]
    fn full_pipeline_produces_all_outputs() {
        let h = harness();
        let wav = write_wav(h._dir.path(), 5000);
        let tc = MockTranscriber::new();
        let sum = MockSummarizer::new();
        let pipe = Pipeline::new(h.cfg.clone(), &tc, Some(&sum)).unwrap();

        let input = AudioRef {
            path: wav,
            duration_ms: None,
            content_hash: None,
            source: AudioSourceKind::File,
        };
        let out = pipe.run(&input, &ProgressSink::silent()).unwrap();

        assert!(!out.transcript.segments.is_empty());
        assert!(out.scene.is_some());
        assert!(out.summary.is_some());
        assert!(out.hardware.is_some());
        assert_eq!(out.transcript.duration_ms, 5000);
        // 输出文件必须真的落盘
        assert!(pipe.files().has_transcript(&out.session_id));
        assert!(pipe.files().read_labels(&out.session_id).unwrap().is_some());
        assert!(pipe
            .files()
            .read_summary_markdown(&out.session_id)
            .unwrap()
            .is_some());
    }

    #[test]
    fn second_run_hits_transcript_cache() {
        let h = harness();
        let wav = write_wav(h._dir.path(), 4000);
        let tc = MockTranscriber::new();
        let sum = MockSummarizer::new();
        let pipe = Pipeline::new(h.cfg.clone(), &tc, Some(&sum)).unwrap();
        let input = AudioRef {
            path: wav.clone(),
            duration_ms: None,
            content_hash: None,
            source: AudioSourceKind::File,
        };

        let first = pipe.run(&input, &ProgressSink::silent()).unwrap();
        assert!(!first.transcript_from_cache, "首次不应命中缓存");

        let second = pipe.run(&input, &ProgressSink::silent()).unwrap();
        assert!(second.transcript_from_cache, "★ 同一音频第二次必须命中缓存");
        assert_eq!(
            first.transcript.segments.len(),
            second.transcript.segments.len()
        );
    }

    #[test]
    fn different_backend_invalidates_cache() {
        let h = harness();
        let wav = write_wav(h._dir.path(), 3000);
        let tc = MockTranscriber::new();
        let sum = MockSummarizer::new();
        let input = AudioRef {
            path: wav,
            duration_ms: None,
            content_hash: None,
            source: AudioSourceKind::File,
        };

        let mut cfg1 = h.cfg.clone();
        cfg1.backend_pref = BackendPref::Force(Backend::Cpu);
        let p1 = Pipeline::new(cfg1, &tc, Some(&sum)).unwrap();
        assert!(!p1.run(&input, &ProgressSink::silent()).unwrap().transcript_from_cache);

        let mut cfg2 = h.cfg.clone();
        cfg2.backend_pref = BackendPref::Force(Backend::Vulkan);
        let p2 = Pipeline::new(cfg2, &tc, Some(&sum)).unwrap();
        let out2 = p2.run(&input, &ProgressSink::silent()).unwrap();
        assert!(
            !out2.transcript_from_cache,
            "★ 换后端必须重新转写(缓存键含 backend)"
        );
    }

    #[test]
    fn summary_can_be_disabled() {
        let h = harness();
        let wav = write_wav(h._dir.path(), 2000);
        let mut cfg = h.cfg.clone();
        cfg.enable_summary = false;
        let tc = MockTranscriber::new();
        let sum = MockSummarizer::new();
        let pipe = Pipeline::new(cfg, &tc, Some(&sum)).unwrap();
        let input = AudioRef {
            path: wav,
            duration_ms: None,
            content_hash: None,
            source: AudioSourceKind::File,
        };
        let out = pipe.run(&input, &ProgressSink::silent()).unwrap();
        assert!(out.summary.is_none());
        assert!(!out.transcript.segments.is_empty(), "转写仍应完成");
    }

    #[test]
    fn works_without_summarizer() {
        let h = harness();
        let wav = write_wav(h._dir.path(), 2000);
        let tc = MockTranscriber::new();
        let pipe = Pipeline::new(h.cfg.clone(), &tc, None).unwrap();
        let input = AudioRef {
            path: wav,
            duration_ms: None,
            content_hash: None,
            source: AudioSourceKind::File,
        };
        let out = pipe.run(&input, &ProgressSink::silent()).unwrap();
        assert!(out.summary.is_none());
        assert!(out.scene.is_none());
    }

    #[test]
    fn missing_voiceprint_models_degrades_gracefully() {
        // ★ 缺声纹模型不应中断整条管线 —— 转写和纪要必须照常产出
        let h = harness();
        let wav = write_wav(h._dir.path(), 3000);
        let tc = MockTranscriber::new();
        let sum = MockSummarizer::new();
        let pipe = Pipeline::new(h.cfg.clone(), &tc, Some(&sum)).unwrap();
        let input = AudioRef {
            path: wav,
            duration_ms: None,
            content_hash: None,
            source: AudioSourceKind::File,
        };
        let out = pipe.run(&input, &ProgressSink::silent()).unwrap();
        assert!(out.transcript.diarize.is_none(), "无模型时不应有说话人信息");
        assert!(!out.transcript.segments.is_empty(), "但转写必须成功");
        assert!(out.summary.is_some(), "纪要也必须成功");
    }

    #[test]
    fn session_is_recorded_in_db() {
        let h = harness();
        let wav = write_wav(h._dir.path(), 2000);
        let tc = MockTranscriber::new();
        let sum = MockSummarizer::new();
        let pipe = Pipeline::new(h.cfg.clone(), &tc, Some(&sum)).unwrap();
        let input = AudioRef {
            path: wav,
            duration_ms: None,
            content_hash: None,
            source: AudioSourceKind::File,
        };
        let out = pipe.run(&input, &ProgressSink::silent()).unwrap();
        let row = pipe.db().get_session(&out.session_id).unwrap().unwrap();
        assert_eq!(row.status, "done");
        assert_eq!(row.duration_ms, 2000);
        assert!(row.scene.is_some());
    }

    #[test]
    fn cancellation_stops_pipeline() {
        let h = harness();
        let wav = write_wav(h._dir.path(), 3000);
        let tc = MockTranscriber::new();
        let sum = MockSummarizer::new();
        let pipe = Pipeline::new(h.cfg.clone(), &tc, Some(&sum)).unwrap();

        let sink = ProgressSink::new(|_| {});
        let canceller = sink.canceller();
        canceller.cancel();

        let input = AudioRef {
            path: wav,
            duration_ms: None,
            content_hash: None,
            source: AudioSourceKind::File,
        };
        let err = pipe.run(&input, &sink).unwrap_err().to_string();
        assert!(err.contains("取消"), "应报告取消而不是静默继续: {err}");
    }

    #[test]
    fn hardware_profile_persisted_and_reused() {
        let h = harness();
        let wav = write_wav(h._dir.path(), 1000);
        let tc = MockTranscriber::new();
        let pipe = Pipeline::new(h.cfg.clone(), &tc, None).unwrap();
        let input = AudioRef {
            path: wav,
            duration_ms: None,
            content_hash: None,
            source: AudioSourceKind::File,
        };
        let out = pipe.run(&input, &ProgressSink::silent()).unwrap();
        let hw = out.hardware.unwrap();
        assert!(hw.cpu_cores >= 1);
        // 硬件画像必须落库(单行表)
        let n: i64 = pipe
            .db()
            .conn()
            .query_row("SELECT COUNT(*) FROM hardware_profile", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn explicit_backend_is_honored() {
        let h = harness();
        let wav = write_wav(h._dir.path(), 1000);
        let mut cfg = h.cfg.clone();
        cfg.backend_pref = BackendPref::Force(Backend::Vulkan);
        let tc = MockTranscriber::new();
        let pipe = Pipeline::new(cfg, &tc, None).unwrap();
        let input = AudioRef {
            path: wav,
            duration_ms: None,
            content_hash: None,
            source: AudioSourceKind::File,
        };
        let out = pipe.run(&input, &ProgressSink::silent()).unwrap();
        assert_eq!(
            out.hardware.unwrap().backend_selected,
            Backend::Vulkan,
            "★ 手动指定后端时不应被静默改写"
        );
    }

    #[test]
    fn labels_persist_across_runs() {
        let h = harness();
        let wav = write_wav(h._dir.path(), 2000);
        let tc = MockTranscriber::new();
        let pipe = Pipeline::new(h.cfg.clone(), &tc, None).unwrap();
        let input = AudioRef {
            path: wav,
            duration_ms: None,
            content_hash: None,
            source: AudioSourceKind::File,
        };
        let out = pipe.run(&input, &ProgressSink::silent()).unwrap();

        // 模拟用户改名
        let mut labels = out.labels.clone();
        labels.ensure([0]);
        labels.rename(0, "张老师");
        pipe.files().write_labels(&labels).unwrap();

        // 再跑一次应读回改名结果
        let out2 = pipe.run(&input, &ProgressSink::silent()).unwrap();
        assert_eq!(
            out2.labels.display_name(0),
            "张老师",
            "★ 改名必须跨运行保持"
        );
    }
}
