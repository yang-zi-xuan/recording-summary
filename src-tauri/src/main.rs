//! Tauri 2 桌面壳。
//!
//! 见技术方案 §2.1:Rust 侧只做**薄封装** —— 全部业务逻辑在 `rs-core`,
//! 这里只负责命令分发、进度事件转发和配置读写。
//!
//! 之所以坚持这个分层:转写一条 45 分钟音频要跑几分钟,在 GUI 里 debug 这个
//! 流程是折磨。CLI 已经把它跑通了,GUI 只是加了个进度条。

// 关掉 Windows 控制台窗口。
//
// ⚠️ **不要**写成 `cfg_attr(not(debug_assertions), ...)`。
// 那样只有 release 构建才隐藏终端,而开发时跑的正是 debug 构建 ——
// 于是"每次打开都弹一个终端"。实测踩过。
//
// 无条件隐藏的代价:debug 构建下 `println!` / `eprintln!` 没有地方输出。
// 但程序本来就用 tracing(见 `init_tracing`),日志写到 stderr ——
// 需要看日志时用 CLI,GUI 不该靠终端窗口来观察。
#![windows_subsystem = "windows"]

use anyhow::Result;
use rs_core::asr::WhisperCppSidecar;
use rs_core::hardware::{self, SidecarLocator};
use rs_core::llm::{self, BlockingSummarizer, LlmConfig, PriceTable};
use rs_core::pipeline::view::{self, ViewKind};
use rs_core::pipeline::{Pipeline, PipelineConfig, Progress, ProgressSink};
use rs_core::store::files::FileStore;
use rs_core::store::{cache, Db};
use rs_core::sync;
use rs_core::types::{AudioRef, AudioSourceKind, BackendPref, DiarizeMode, ModelTier, SpeakerLabels};
use rs_core::voiceprint::VoiceprintStore;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{Emitter, Manager, State};

// ---------------------------------------------------------------------------
// 应用状态
// ---------------------------------------------------------------------------

struct AppState {
    data_dir: PathBuf,
    /// 当前运行的取消句柄。None 表示没有任务在跑。
    cancel: Mutex<Option<rs_core::pipeline::Canceller>>,
}

impl AppState {
    fn paths(&self) -> Paths {
        resolve_paths(&self.data_dir)
    }

    fn db(&self) -> Result<Db> {
        Db::open(&self.data_dir.join("cache.db"))
    }

    fn files(&self) -> Result<FileStore> {
        let f = FileStore::new(self.data_dir.join("store"));
        f.ensure_dirs()?;
        Ok(f)
    }
}

/// 二进制与模型的解析规则与 CLI 保持一致:
/// 资源目录(模型与二进制)的解析结果。
///
/// ⚠️ 解析顺序见 [`rs_core::paths`]。**不要**用裸的相对路径 ——
/// 双击 exe 启动时工作目录不是项目根目录,相对路径会失效,
/// 界面会显示「没有模型」。
struct Paths {
    data_dir: PathBuf,
    binaries: PathBuf,
    models: PathBuf,
}

fn resolve_paths(data_dir: &std::path::Path) -> Paths {
    Paths {
        data_dir: data_dir.to_path_buf(),
        binaries: rs_core::paths::find_binaries_dir(data_dir)
            .unwrap_or_else(|| data_dir.join("binaries")),
        models: rs_core::paths::find_models_dir(data_dir)
            .unwrap_or_else(|| data_dir.join("models")),
    }
}

// ---------------------------------------------------------------------------
// 返回给前端的结构
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct HardwareInfo {
    backend: String,
    backend_label: String,
    device_name: Option<String>,
    cpu_cores: usize,
    vram_gb: Option<f32>,
    model_recommended: String,
    model_file: String,
    model_ready: bool,
    /// 各后端的可用性,给设置页展示
    backends: Vec<BackendRow>,
    binaries_dir: String,
    models_dir: String,
    data_dir: String,
    ffmpeg: Option<String>,
}

#[derive(Serialize)]
struct BackendRow {
    name: String,
    label: String,
    has_binary: bool,
    hardware_available: bool,
    usable: bool,
}

#[derive(Serialize)]
struct SessionCard {
    id: String,
    short_id: String,
    title: String,
    created_at: i64,
    duration_ms: u64,
    duration_text: String,
    scene: Option<String>,
    scene_label: Option<String>,
    status: String,
    has_summary: bool,
    speaker_count: Option<u32>,
    /// 对应的工程目录是否还在。
    ///
    /// 历史记录与工程是**两套独立存储**,删掉工程后会话记录仍保留
    /// (这是刻意的:它记录了"这段录音处理过")。
    /// 界面据此把那一条标成「工程已删除」,免得点进去什么都没有。
    project_exists: bool,
}

#[derive(Serialize)]
struct SessionDetail {
    id: String,
    title: String,
    duration_text: String,
    transcript: String,
    timeline: String,
    plain: String,
    srt: String,
    summary: Option<String>,
    scene: Option<String>,
    scene_label: Option<String>,
    scene_confidence: Option<f32>,
    scene_low_confidence: bool,
    scene_evidence: Option<String>,
    summary_stale: bool,
    labels: Vec<SpeakerRow>,
    used_map_reduce: bool,
    usage_text: Option<String>,
    /// 原始录音路径(处理时拖进来的那个文件,在应用目录之外)。
    ///
    /// 删除工程时界面会问"要不要一并删掉它" —— 所以要让用户看见是哪一份。
    /// 可能是 `None`(会话记录不在,或当时没记)。
    audio_path: Option<String>,
}

#[derive(Serialize)]
struct SpeakerRow {
    id: u32,
    name: String,
    color: String,
    profile_id: Option<String>,
    talk_time_text: String,
    segment_count: u32,
}

#[derive(Serialize)]
struct LlmInfo {
    provider: String,
    base_url: String,
    model: String,
    model_hint: String,
    key_masked: Option<String>,
    key_from_env: bool,
    presets: Vec<PresetRow>,
}

#[derive(Serialize)]
struct PresetRow {
    name: String,
    url: String,
    models: Vec<String>,
}

#[derive(Serialize)]
struct SyncInfo {
    url: String,
    username: String,
    has_password: bool,
    remote_dir: String,
    root: String,
    local_dir: String,
}

#[derive(Serialize)]
struct ProfileRow {
    id: String,
    name: String,
    sample_count: usize,
    total_minutes: u64,
    model: String,
}

#[derive(Serialize, Clone)]
struct RunOutcomeDto {
    session_id: String,
    short_id: String,
    duration_text: String,
    segment_count: usize,
    speaker_count: Option<u32>,
    backend_label: String,
    model: String,
    from_cache: bool,
    scene_label: Option<String>,
    scene_confidence: Option<f32>,
    scene_low_confidence: bool,    summary: Option<String>,
}

#[derive(Deserialize)]
struct RunRequest {
    input: String,
    language: Option<String>,
    model: Option<String>,
    backend: Option<String>,
    speakers: Option<u8>,
    no_diarize: bool,
    no_summary: bool,
    terms: Vec<String>,
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn format_ms(ms: u64) -> String {
    let total = ms / 1000;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// 允许用 ID 前缀定位会话(哈希太长,手打不现实)。
fn resolve_session(files: &FileStore, prefix: &str) -> Result<String> {
    if files.has_transcript(prefix) {
        return Ok(prefix.to_string());
    }
    let dir = files.root().join("transcript");
    let mut matches = Vec::new();
    if dir.is_dir() {
        for e in walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
            if e.file_type().is_file() {
                if let Some(stem) = e.path().file_stem().and_then(|s| s.to_str()) {
                    if stem.starts_with(prefix) {
                        matches.push(stem.to_string());
                    }
                }
            }
        }
    }
    match matches.len() {
        0 => anyhow::bail!("找不到会话: {prefix}"),
        1 => Ok(matches.remove(0)),
        _ => anyhow::bail!("前缀匹配到多个会话,请多给几位"),
    }
}

/// 把 anyhow 错误转成前端能显示的字符串。
fn err_str(e: anyhow::Error) -> String {
    format!("{e:#}")
}

// ---------------------------------------------------------------------------
// 命令:环境与硬件
// ---------------------------------------------------------------------------

#[tauri::command]
fn probe_hardware(state: State<AppState>) -> Result<HardwareInfo, String> {
    let p = state.paths();
    let locator = SidecarLocator::new(&p.binaries);
    let cores = hardware::cpu_cores();
    let vram = hardware::nvidia_vram_gb();

    let mut backends = Vec::new();
    for b in rs_core::types::Backend::PRIORITY {
        let has_binary = locator.whisper_cli(b).is_some();
        let hw = hardware::available(b);
        backends.push(BackendRow {
            name: b.as_str().to_string(),
            label: b.label().to_string(),
            has_binary,
            hardware_available: hw,
            usable: has_binary && hw,
        });
    }

    let selected = hardware::select_backend(BackendPref::Auto, &locator)
        .unwrap_or(rs_core::types::Backend::Cpu);
    let model = hardware::recommend_model(selected, vram, cores);
    let model_file = p.models.join(model.file_name());
    let device = match selected {
        rs_core::types::Backend::Cuda => {
            hardware::nvidia_smi_devices().and_then(|d| d.into_iter().next())
        }
        _ => None,
    };

    Ok(HardwareInfo {
        backend: selected.as_str().to_string(),
        backend_label: selected.label().to_string(),
        device_name: device,
        cpu_cores: cores,
        vram_gb: vram,
        model_recommended: model.label().to_string(),
        model_file: model.file_name().to_string(),
        model_ready: model_file.is_file(),
        backends,
        binaries_dir: p.binaries.to_string_lossy().to_string(),
        models_dir: p.models.to_string_lossy().to_string(),
        data_dir: p.data_dir.to_string_lossy().to_string(),
        ffmpeg: rs_core::audio::find_ffmpeg(None, Some(&p.binaries))
            .map(|p| p.to_string_lossy().to_string()),
    })
}

/// 列出模型目录里已下载的档位。
#[tauri::command]
fn list_models(state: State<AppState>) -> Vec<ModelRow> {
    let p = state.paths();
    [
        ModelTier::Tiny,
        ModelTier::Base,
        ModelTier::Small,
        ModelTier::Medium,
        ModelTier::LargeV3Turbo,
    ]
    .into_iter()
    .map(|t| {
        let f = p.models.join(t.file_name());
        let size = std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
        ModelRow {
            tier: t.label().to_string(),
            file: t.file_name().to_string(),
            present: f.is_file(),
            size_mb: size / 1_048_576,
            approx_mb: t.approx_mb(),
        }
    })
    .collect()
}

#[derive(Serialize)]
struct ModelRow {
    tier: String,
    file: String,
    present: bool,
    size_mb: u64,
    approx_mb: u64,
}

// ---------------------------------------------------------------------------
// 命令:会话
// ---------------------------------------------------------------------------

#[tauri::command]
fn list_sessions(state: State<AppState>, limit: Option<usize>) -> Result<Vec<SessionCard>, String> {
    let db = state.db().map_err(err_str)?;
    let files = state.files().map_err(err_str)?;
    let rows = db.list_sessions(limit.unwrap_or(100)).map_err(err_str)?;

    // 一次性建出"工程 ID → 目录"的表,避免每行都去遍历 projects/
    let projects = {
        use rs_core::project::ProjectStore;
        let store = ProjectStore::new(files.root());
        store
            .list()
            .unwrap_or_default()
            .into_iter()
            .map(|p| (p.meta.id.clone(), p.dir.clone()))
            .collect::<std::collections::HashMap<_, _>>()
    };

    Ok(rows
        .into_iter()
        .map(|r| {
            let labels = files.read_labels(&r.id).ok().flatten();
            let speaker_count = labels.as_ref().map(|l| l.labels.len() as u32);
            let has_summary = files
                .read_summary_markdown(&r.id)
                .ok()
                .flatten()
                .is_some();
            SessionCard {
                short_id: short_id(&r.id),
                title: r
                    .title
                    .clone()
                    .unwrap_or_else(|| "(未命名)".into()),
                created_at: r.created_at,
                duration_ms: r.duration_ms,
                duration_text: format_ms(r.duration_ms),
                scene: r.scene.map(|s| format!("{s:?}").to_lowercase()),
                scene_label: r.scene.map(|s| s.label().to_string()),
                status: r.status.clone(),
                has_summary,
                speaker_count,
                // 工程目录还在吗 —— 删了工程之后会话记录仍保留,
                // 界面据此把它标成「工程已删除」
                project_exists: projects
                    .get(&r.id)
                    .map(|d| d.is_dir())
                    .unwrap_or(false),
                id: r.id,
            }
        })
        .collect())
}

#[tauri::command]
fn get_session(state: State<AppState>, id: String) -> Result<SessionDetail, String> {
    let files = state.files().map_err(err_str)?;
    let full_id = resolve_session(&files, &id).map_err(err_str)?;

    let transcript = files
        .read_transcript(&full_id)
        .map_err(err_str)?
        .ok_or_else(|| format!("会话 {full_id} 没有转写文件"))?;
    let labels = files
        .read_labels(&full_id)
        .map_err(err_str)?
        .unwrap_or_else(|| SpeakerLabels::new(full_id.clone()));

    let summary_md = files.read_summary_markdown(&full_id).map_err(err_str)?;
    let summary_meta = files.read_summary_meta(&full_id).map_err(err_str)?;
    // 原始录音路径 —— 删除工程时界面会问"要不要一并删掉它"
    let audio_path = state
        .db()
        .ok()
        .and_then(|db| db.get_session(&full_id).ok().flatten())
        .and_then(|s| s.audio_local_path);
    let db = state.db().map_err(err_str)?;
    let row = db.get_session(&full_id).map_err(err_str)?;

    let scene = row.as_ref().and_then(|r| r.scene);
    let conf = row.as_ref().and_then(|r| r.scene_confidence);

    let summary_stale = summary_meta
        .as_ref()
        .map(|m| m.is_stale(&labels))
        .unwrap_or(false);

    // 说话人统计:时长与段数
    let mut stats: std::collections::BTreeMap<u32, (u64, u32)> = Default::default();
    for s in &transcript.segments {
        if let Some(sid) = s.speaker_id {
            let e = stats.entry(sid).or_insert((0, 0));
            e.0 += s.duration_ms();
            e.1 += 1;
        }
    }

    let speaker_rows: Vec<SpeakerRow> = labels
        .labels
        .iter()
        .map(|(sid, l)| {
            let (ms, n) = stats.get(sid).copied().unwrap_or((0, 0));
            SpeakerRow {
                id: *sid,
                name: l.display_name.clone(),
                color: l.color.clone(),
                profile_id: l.profile_id.clone(),
                talk_time_text: if ms > 0 { format_ms(ms) } else { "—".into() },
                segment_count: n,
            }
        })
        .collect();

    let usage_text = summary_meta.as_ref().map(|m| {
        let price = PriceTable::cny();
        format!(
            "输入 {} / 输出 {} / 缓存命中 {} tokens · 费用 {}",
            m.usage.input,
            m.usage.output,
            m.usage.cached_input,
            price.format_cost(&m.model, &m.usage)
        )
    });

    Ok(SessionDetail {
        title: row
            .as_ref()
            .and_then(|r| r.title.clone())
            .unwrap_or_else(|| short_id(&full_id)),
        duration_text: format_ms(transcript.duration_ms),
        transcript: view::render_dialogue(&transcript.segments, &labels),
        timeline: view::render_timeline(&transcript.segments, &labels),
        plain: view::render_plain(&transcript.segments, &labels),
        srt: view::render_srt(&transcript.segments, &labels),
        summary: summary_md,
        scene: scene.map(|s| format!("{s:?}").to_lowercase()),
        scene_label: scene.map(|s| s.label().to_string()),
        scene_confidence: conf,
        scene_low_confidence: conf.map(|c| c < 0.6).unwrap_or(false),
        scene_evidence: row.as_ref().and_then(|_| None),
        summary_stale,
        labels: speaker_rows,
        used_map_reduce: summary_meta.as_ref().map(|m| m.used_map_reduce).unwrap_or(false),
        usage_text,
        id: full_id,
        audio_path,
    })
}

/// 改名。**只改标签,不重新转写** —— 所以是秒级的。
#[tauri::command]
fn rename_speaker(
    state: State<AppState>,
    id: String,
    speaker_id: u32,
    name: String,
) -> Result<SessionDetail, String> {
    let files = state.files().map_err(err_str)?;
    let full_id = resolve_session(&files, &id).map_err(err_str)?;

    let transcript = files
        .read_transcript(&full_id)
        .map_err(err_str)?
        .ok_or_else(|| "该会话没有转写文件".to_string())?;

    if !transcript.has_speakers() {
        return Err(
            "该会话的转写里没有发言人信息,无法改名。\n\n\
             可能原因:\n\
             · 处理时跳过了说话人区分\n\
             · 声纹模型未就位,该阶段被自动跳过\n\n\
             解决:把 segmentation-3.0.onnx 与 3dspeaker.onnx 放到模型目录后重新处理。"
                .into(),
        );
    }

    let mut labels = files
        .read_labels(&full_id)
        .map_err(err_str)?
        .unwrap_or_else(|| SpeakerLabels::new(full_id.clone()));
    let ids: Vec<u32> = transcript
        .segments
        .iter()
        .filter_map(|s| s.speaker_id)
        .collect();
    labels.ensure(ids);

    if !labels.rename(speaker_id, &name) {
        // 名字没变,不算错误
    }
    files.write_labels(&labels).map_err(err_str)?;

    get_session(state, full_id)
}

/// 显示指定视图(供"导出/复制"用)。
#[tauri::command]
fn render_view(state: State<AppState>, id: String, kind: String) -> Result<String, String> {
    let files = state.files().map_err(err_str)?;
    let full_id = resolve_session(&files, &id).map_err(err_str)?;
    let transcript = files
        .read_transcript(&full_id)
        .map_err(err_str)?
        .ok_or_else(|| "该会话没有转写文件".to_string())?;
    let labels = files
        .read_labels(&full_id)
        .map_err(err_str)?
        .unwrap_or_else(|| SpeakerLabels::new(full_id.clone()));
    let k = ViewKind::parse(&kind).ok_or_else(|| format!("未知视图: {kind}"))?;
    Ok(k.render(&transcript.segments, &labels))
}

/// 写文本到用户选择的路径。
#[tauri::command]
fn save_text(path: String, content: String) -> Result<(), String> {
    std::fs::write(&path, content).map_err(|e| format!("写入 {path} 失败: {e}"))
}

// ---------------------------------------------------------------------------
// 命令:处理
// ---------------------------------------------------------------------------

/// 启动处理。**立即返回**,进度通过 `pipeline://progress` 事件推送。
///
/// 这样做的原因:转写可能跑几十分钟,必须避免阻塞,而且要能取消。
#[tauri::command]
fn start_run(
    app: tauri::AppHandle,
    state: State<AppState>,
    req: RunRequest,
) -> Result<(), String> {
    // 不允许并发跑多个任务
    {
        let guard = state.cancel.lock().unwrap();
        if guard.is_some() {
            return Err("已有任务在运行,请先取消或等待完成。".into());
        }
    }

    let input_path = PathBuf::from(&req.input);
    if !input_path.is_file() {
        return Err(format!("文件不存在: {}", req.input));
    }

    let p = state.paths();
    let data_dir = p.data_dir.clone();

    let mut cfg = PipelineConfig::new(&p.data_dir)
        .with_binaries(&p.binaries)
        .with_models(&p.models);
    cfg.hotwords = req.terms.clone();
    cfg.enable_summary = !req.no_summary;
    cfg.language = match req.language.as_deref() {
        None | Some("") | Some("auto") => None,
        Some(l) => Some(l.to_string()),
    };
    if req.no_diarize {
        cfg.enable_diarize = false;
    } else if let Some(n) = req.speakers {
        cfg.diarize = DiarizeMode::Fixed(n.max(1));
    }
    if let Some(b) = &req.backend {
        if let Some(backend) = rs_core::types::Backend::parse(b) {
            cfg.backend_pref = BackendPref::Force(backend);
        }
    }
    if let Some(m) = &req.model {
        cfg.model_override = ModelTier::parse(m);
    }

    let sink = ProgressSink::new({
        let app = app.clone();
        move |prog| {
            let payload = progress_to_json(&prog);
            let _ = app.emit("pipeline://progress", payload);
        }
    });
    let canceller = sink.canceller();

    {
        let mut guard = state.cancel.lock().unwrap();
        *guard = Some(canceller);
    }

    let bins = p.binaries.clone();
    let models = p.models.clone();
    let app2 = app.clone();

    // 在独立线程里跑 —— 管线本身是同步的
    std::thread::spawn(move || {
        let result = run_pipeline(&data_dir, &bins, &models, cfg_with(cfg), &input_path, &sink);

        match result {
            Ok(outcome) => {
                let _ = app2.emit("pipeline://done", outcome);
            }
            Err(e) => {
                let _ = app2.emit("pipeline://error", format!("{e:#}"));
            }
        }

        // 清掉取消句柄
        if let Some(st) = app2.try_state::<AppState>() {
            let mut guard = st.cancel.lock().unwrap();
            *guard = None;
        }
    });

    Ok(())
}

/// 让闭包捕获所有权的小辅助(避免生命周期纠缠)。
fn cfg_with(c: PipelineConfig) -> PipelineConfig {
    c
}

fn run_pipeline(
    data_dir: &std::path::Path,
    binaries: &std::path::Path,
    models: &std::path::Path,
    cfg: PipelineConfig,
    input_path: &std::path::Path,
    sink: &ProgressSink,
) -> Result<RunOutcomeDto> {
    let transcriber = WhisperCppSidecar::new(SidecarLocator::new(binaries));

    // 总结器:Key 没配就跳过,不阻塞转写
    let summarizer = build_summarizer(data_dir).ok();
    let sum_ref = summarizer.as_ref().map(|s| s as &dyn llm::Summarizer);

    let pipe = Pipeline::new(cfg, &transcriber, sum_ref)?;

    let input = AudioRef {
        path: input_path.to_path_buf(),
        duration_ms: None,
        content_hash: None,
        source: AudioSourceKind::File,
    };
    let out = pipe.run(&input, sink)?;

    let _ = models;
    Ok(RunOutcomeDto {
        short_id: short_id(&out.session_id),
        duration_text: format_ms(out.transcript.duration_ms),
        segment_count: out.transcript.segments.len(),
        speaker_count: out.transcript.diarize.as_ref().map(|d| d.num_speakers_detected),
        backend_label: out
            .hardware
            .as_ref()
            .map(|h| h.summary_line())
            .unwrap_or_default(),
        model: out
            .hardware
            .as_ref()
            .map(|h| h.model_recommended.label().to_string())
            .unwrap_or_default(),
        from_cache: out.transcript_from_cache,
        scene_label: out.scene.as_ref().map(|v| v.scene.label().to_string()),
        scene_confidence: out.scene.as_ref().map(|v| v.confidence),
        scene_low_confidence: out.scene.as_ref().map(|v| v.is_low_confidence()).unwrap_or(false),
        summary: out.summary.as_ref().map(|s| s.content_md.clone()),
        session_id: out.session_id,
    })
}

fn build_summarizer(data_dir: &std::path::Path) -> Result<BlockingSummarizer> {
    let db = Db::open(&data_dir.join("cache.db"))?;
    let mut cfg = llm_config_from_db(&db)?;
    cfg.api_key = llm::load_api_key().ok().flatten();
    BlockingSummarizer::new(cfg, PriceTable::cny())
}

fn llm_config_from_db(db: &Db) -> Result<LlmConfig> {
    let mut cfg = LlmConfig::default();
    if let Some(v) = db.get_setting("llm.base_url")? {
        cfg.base_url = v;
    }
    if let Some(v) = db.get_setting("llm.model")? {
        cfg.model = v;
    }
    if let Some(v) = db.get_setting("llm.provider")? {
        cfg.provider = v;
    }
    Ok(cfg)
}

fn progress_to_json(p: &Progress) -> serde_json::Value {
    use serde_json::json;
    match p {
        Progress::StageStart(s, _) => json!({ "kind": "stage_start", "stage": s.label() }),
        Progress::StagePct(s, pct) => {
            json!({ "kind": "stage_pct", "stage": s.label(), "pct": pct })
        }
        Progress::Transcribe {
            audio_ms_done,
            audio_ms_total,
        } => json!({
            "kind": "transcribe",
            "done_ms": audio_ms_done,
            "total_ms": audio_ms_total,
        }),
        Progress::CacheHit(s) => json!({ "kind": "cache_hit", "stage": s.label() }),
        Progress::Note(m) => json!({ "kind": "note", "message": m }),
    }
}

#[tauri::command]
fn cancel_run(state: State<AppState>) -> Result<bool, String> {
    let guard = state.cancel.lock().unwrap();
    match guard.as_ref() {
        Some(c) => {
            c.cancel();
            Ok(true)
        }
        None => Ok(false),
    }
}

#[tauri::command]
fn is_running(state: State<AppState>) -> bool {
    state.cancel.lock().unwrap().is_some()
}

// ---------------------------------------------------------------------------
// 命令:LLM 配置
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_llm_config(state: State<AppState>) -> Result<LlmInfo, String> {
    let db = state.db().map_err(err_str)?;
    let cfg = llm_config_from_db(&db).map_err(err_str)?;
    let key = llm::load_api_key().ok().flatten();
    let from_env = llm::keyring::api_key_from_env().is_some();

    Ok(LlmInfo {
        provider: cfg.provider.clone(),
        base_url: cfg.base_url.clone(),
        model: cfg.model.clone(),
        model_hint: cfg.model_hint().to_string(),
        key_masked: key.as_deref().map(llm::keyring::mask),
        key_from_env: from_env,
        presets: LlmConfig::presets()
            .into_iter()
            .map(|(name, url, models)| PresetRow {
                name: name.to_string(),
                url: url.to_string(),
                models: models.into_iter().map(|s| s.to_string()).collect(),
            })
            .collect(),
    })
}

#[tauri::command]
fn set_llm_config(
    state: State<AppState>,
    base_url: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    api_key: Option<String>,
) -> Result<(), String> {
    let db = state.db().map_err(err_str)?;
    if let Some(v) = base_url {
        db.set_setting("llm.base_url", &v).map_err(err_str)?;
    }
    if let Some(v) = model {
        db.set_setting("llm.model", &v).map_err(err_str)?;
    }
    if let Some(v) = provider {
        db.set_setting("llm.provider", &v).map_err(err_str)?;
    }
    // API Key 走凭据管理器,不进数据库
    if let Some(k) = api_key {
        if k.trim().is_empty() {
            let _ = llm::delete_api_key();
        } else {
            llm::store_api_key(k.trim()).map_err(err_str)?;
        }
    }
    Ok(())
}

#[tauri::command]
fn clear_api_key() -> Result<(), String> {
    llm::delete_api_key().map_err(err_str)?;
    Ok(())
}

#[tauri::command]
async fn test_llm(state: State<'_, AppState>) -> Result<String, String> {
    let db = state.db().map_err(err_str)?;
    let mut cfg = llm_config_from_db(&db).map_err(err_str)?;
    cfg.api_key = llm::load_api_key().map_err(err_str)?;
    let sum = BlockingSummarizer::new(cfg, PriceTable::cny()).map_err(err_str)?;
    // 放到阻塞线程池里跑,避免卡住 UI 事件循环
    tauri::async_runtime::spawn_blocking(move || sum.test_connection_blocking())
        .await
        .map_err(|e| format!("任务失败: {e}"))?
        .map_err(err_str)
}

// ---------------------------------------------------------------------------
// 命令:WebDAV 同步
// ---------------------------------------------------------------------------

const KEYRING_WEBDAV: &str = "webdav-password";

fn sync_info_from_db(db: &Db) -> Result<SyncInfo> {
    let mut cfg = sync::WebDavConfig::default();
    if let Some(v) = db.get_setting("webdav.url")? {
        cfg.base_url = v;
    }
    if let Some(v) = db.get_setting("webdav.username")? {
        cfg.username = v;
    }
    if let Some(v) = db.get_setting("webdav.dir")? {
        cfg.remote_dir = v;
    }
    let has_pw = keyring::Entry::new(llm::KEYRING_SERVICE, KEYRING_WEBDAV)
        .ok()
        .and_then(|e| e.get_password().ok())
        .map(|p| !p.is_empty())
        .unwrap_or(false);

    Ok(SyncInfo {
        root: cfg.root(),
        url: cfg.base_url,
        username: cfg.username,
        has_password: has_pw,
        remote_dir: cfg.remote_dir,
        local_dir: String::new(),
    })
}

fn webdav_config(db: &Db) -> Result<sync::WebDavConfig> {
    let mut cfg = sync::WebDavConfig::default();
    if let Some(v) = db.get_setting("webdav.url")? {
        cfg.base_url = v;
    }
    if let Some(v) = db.get_setting("webdav.username")? {
        cfg.username = v;
    }
    if let Some(v) = db.get_setting("webdav.dir")? {
        cfg.remote_dir = v;
    }
    cfg.password = std::env::var("RECSUM_WEBDAV_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            keyring::Entry::new(llm::KEYRING_SERVICE, KEYRING_WEBDAV)
                .ok()
                .and_then(|e| e.get_password().ok())
        })
        .unwrap_or_default();
    Ok(cfg)
}

#[tauri::command]
fn get_sync_config(state: State<AppState>) -> Result<SyncInfo, String> {
    let db = state.db().map_err(err_str)?;
    let files = state.files().map_err(err_str)?;
    let mut info = sync_info_from_db(&db).map_err(err_str)?;
    info.local_dir = files.root().to_string_lossy().to_string();
    Ok(info)
}

#[tauri::command]
fn set_sync_config(
    state: State<AppState>,
    url: Option<String>,
    username: Option<String>,
    remote_dir: Option<String>,
    password: Option<String>,
) -> Result<String, String> {
    let db = state.db().map_err(err_str)?;

    // ★ 允许把一整条地址粘进"服务地址"框。
    //
    // 用户脑子里想的是 `https://cloud.example.com/seafdav/recording_summary`
    // 这一条,而不是"服务地址"+"远端目录"两个框。不拆的话会拼出
    // .../seafdav/recording_summary/recording_summary,而报错是 404,
    // 完全指不到问题所在。
    let mut url = url;
    let mut remote_dir = remote_dir;
    let mut note = String::new();
    if let Some(u) = url.clone() {
        if let Some((base, extracted)) = sync::split_webdav_url(&u) {
            let dir_is_default = remote_dir
                .as_deref()
                .map(|d| d.trim().is_empty() || d == "recording-summary")
                .unwrap_or(true);
            if dir_is_default {
                url = Some(base);
                remote_dir = Some(extracted.clone());
                note = format!("已从地址里识别出远端目录:{extracted}");
            } else {
                // 用户另外填了目录 —— 以他填的为准,但要说一声
                url = Some(base);
                note = format!(
                    "地址里含子目录「{extracted}」,但远端目录已填「{}」,以后者为准",
                    remote_dir.as_deref().unwrap_or("")
                );
            }
        }
    }

    if let Some(v) = url {
        db.set_setting("webdav.url", &v).map_err(err_str)?;
    }
    if let Some(v) = username {
        db.set_setting("webdav.username", &v).map_err(err_str)?;
    }
    if let Some(v) = remote_dir {
        db.set_setting("webdav.dir", &v).map_err(err_str)?;
    }
    if let Some(pw) = password {
        if pw.is_empty() {
            if let Ok(e) = keyring::Entry::new(llm::KEYRING_SERVICE, KEYRING_WEBDAV) {
                let _ = e.delete_credential();
            }
        } else {
            keyring::Entry::new(llm::KEYRING_SERVICE, KEYRING_WEBDAV)
                .map_err(|e| format!("无法访问凭据管理器: {e}"))?
                .set_password(&pw)
                .map_err(|e| format!("保存密码失败: {e}"))?;
        }
    }

    // 返回一句提示(可能为空),让界面能告诉用户"地址被拆过了"
    Ok(note)
}

/// 把界面上的当前输入合并进数据库里的配置。
///
/// **为什么需要这个:** 「测试连接」必须测**用户刚填的值**,而不是
/// 数据库里已保存的值。否则用户填完用户名直接点测试,得到的是
/// "用户名为空"—— 他明明填了,只是还没保存。这个坑真实踩过。
///
/// 处理规则:
/// - `url` / `username` 给了非空值就用它,否则退回数据库
/// - `password` 为空表示"没改",用凭据管理器里的
/// - `remote_dir` 特殊:为空时从 url 里拆(见 [`sync::split_webdav_url`])
fn effective_webdav_config(
    db: &Db,
    url: Option<&str>,
    username: Option<&str>,
    remote_dir: Option<&str>,
    password: Option<&str>,
) -> Result<sync::WebDavConfig> {
    let mut cfg = webdav_config(db)?;

    if let Some(u) = url.map(str::trim).filter(|s| !s.is_empty()) {
        // 允许整条地址粘进来 —— 自动拆出服务地址与远端目录
        match sync::split_webdav_url(u) {
            Some((base, extracted)) => {
                cfg.base_url = base;
                // 用户另外填了远端目录时以他填的为准
                let dir_given = remote_dir.map(str::trim).filter(|s| !s.is_empty());
                if dir_given.is_none() {
                    cfg.remote_dir = extracted;
                }
            }
            None => cfg.base_url = u.to_string(),
        }
    }

    if let Some(n) = username.map(str::trim).filter(|s| !s.is_empty()) {
        cfg.username = n.to_string();
    }
    if let Some(d) = remote_dir.map(str::trim).filter(|s| !s.is_empty()) {
        cfg.remote_dir = d.to_string();
    }
    if let Some(p) = password.filter(|s| !s.is_empty()) {
        cfg.password = p.to_string();
    }

    Ok(cfg)
}

#[tauri::command]
async fn test_sync(
    state: State<'_, AppState>,
    url: Option<String>,
    username: Option<String>,
    remote_dir: Option<String>,
    password: Option<String>,
) -> Result<String, String> {
    let db = state.db().map_err(err_str)?;
    let files = state.files().map_err(err_str)?;
    let cfg = effective_webdav_config(
        &db,
        url.as_deref(),
        username.as_deref(),
        remote_dir.as_deref(),
        password.as_deref(),
    )
    .map_err(err_str)?;

    tauri::async_runtime::spawn_blocking(move || {
        let s = sync::Syncer::new(cfg, &files)?;
        s.test_connection()
    })
    .await
    .map_err(|e| format!("任务失败: {e}"))?
    .map_err(err_str)
}

#[tauri::command]
async fn sync_plan(
    state: State<'_, AppState>,
    url: Option<String>,
    username: Option<String>,
    remote_dir: Option<String>,
    password: Option<String>,
) -> Result<Vec<SyncPlanRow>, String> {
    let db = state.db().map_err(err_str)?;
    let files = state.files().map_err(err_str)?;
    // 同样用界面上的当前值 —— "查看计划"不该要求先保存
    let cfg = effective_webdav_config(
        &db,
        url.as_deref(),
        username.as_deref(),
        remote_dir.as_deref(),
        password.as_deref(),
    )
    .map_err(err_str)?;
    tauri::async_runtime::spawn_blocking(move || -> Result<Vec<SyncPlanRow>> {
        let s = sync::Syncer::new(cfg, &files)?;
        let m = sync::manifest::Manifest::load_or_new(&files.manifest_path())?;
        let plan = s.plan(&m)?;
        Ok(plan
            .into_iter()
            .map(|i| SyncPlanRow {
                path: i.rel_path,
                action: match i.action {
                    sync::Action::Upload => "上传".into(),
                    sync::Action::Download => "下载".into(),
                    sync::Action::Skip => "跳过".into(),
                    sync::Action::DeleteRemote => "删除".into(),
                },
                size: i.local_size,
            })
            .collect())
    })
    .await
    .map_err(|e| format!("任务失败: {e}"))?
    .map_err(err_str)
}

#[derive(Serialize)]
struct SyncPlanRow {
    path: String,
    action: String,
    size: u64,
}

#[tauri::command]
async fn run_sync(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    url: Option<String>,
    username: Option<String>,
    remote_dir: Option<String>,
    password: Option<String>,
) -> Result<String, String> {
    let db = state.db().map_err(err_str)?;
    let files = state.files().map_err(err_str)?;
    // 用界面上的当前值 —— 与「测试连接」「查看计划」保持一致,
    // 否则用户会看到"测试通过但同步报用户名空"这种自相矛盾的结果
    let cfg = effective_webdav_config(
        &db,
        url.as_deref(),
        username.as_deref(),
        remote_dir.as_deref(),
        password.as_deref(),
    )
    .map_err(err_str)?;
    let sel = sync_selection(&db);

    tauri::async_runtime::spawn_blocking(move || -> Result<String> {
        let s = sync::Syncer::new(cfg, &files)?.with_selection(sel);
        let report = s.run(&|path, i, total| {
            let _ = app.emit(
                "sync://progress",
                serde_json::json!({ "path": path, "done": i, "total": total }),
            );
        })?;
        let mut out = report.summary();
        if !report.failed.is_empty() {
            out.push_str("\n\n失败明细:\n");
            for (p, e) in &report.failed {
                out.push_str(&format!("  {p}\n    {e}\n"));
            }
        }
        Ok(out)
    })
    .await
    .map_err(|e| format!("任务失败: {e}"))?
    .map_err(err_str)
}

#[tauri::command]
fn audio_sync_hint(state: State<AppState>) -> Result<String, String> {
    let files = state.files().map_err(err_str)?;
    Ok(sync::audio_sync_hint_for(files.root()))
}

// ---------------------------------------------------------------------------
// 命令:同步范围与清单
// ---------------------------------------------------------------------------

/// 读取同步选择配置,缺失时给默认值。
///
/// 存在 settings 表的 `sync.selection` 里(JSON)。放这里而不是单独文件,
/// 是因为它属于"用户偏好"而不是"数据",跟着数据库走最省事。
fn sync_selection(db: &Db) -> sync::selection::SyncSelection {
    db.get_setting("sync.selection")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(sync::selection::SyncSelection::all)
}

fn save_sync_selection(db: &Db, sel: &sync::selection::SyncSelection) -> Result<()> {
    db.set_setting("sync.selection", &serde_json::to_string(sel)?)
}

/// 同步选择(给界面显示当前设置)。
#[derive(Serialize, Deserialize, Clone)]
struct SyncSelectionDto {
    /// both | upload | download
    direction: String,
    /// sync | skip
    audio: String,
    /// keep | text | mirror
    deletion: String,
    /// 逐项排除的路径
    user_excludes: Vec<String>,
    /// 人类可读描述
    describe: String,
}

impl From<sync::selection::SyncSelection> for SyncSelectionDto {
    fn from(s: sync::selection::SyncSelection) -> Self {
        use sync::selection::{AudioPolicy, DeletionPolicy, Direction};
        Self {
            direction: match s.direction {
                Direction::Both => "both",
                Direction::UploadOnly => "upload",
                Direction::DownloadOnly => "download",
            }
            .into(),
            audio: match s.audio {
                AudioPolicy::Sync => "sync",
                AudioPolicy::Skip => "skip",
            }
            .into(),
            deletion: match s.deletion {
                DeletionPolicy::Keep => "keep",
                DeletionPolicy::TextOnly => "text",
                DeletionPolicy::Mirror => "mirror",
            }
            .into(),
            user_excludes: s.user_excludes.clone(),
            describe: s.describe(),
        }
    }
}

#[tauri::command]
fn get_sync_selection(state: State<AppState>) -> Result<SyncSelectionDto, String> {
    let db = state.db().map_err(err_str)?;
    Ok(sync_selection(&db).into())
}

#[tauri::command]
fn set_sync_selection(
    state: State<AppState>,
    direction: Option<String>,
    audio: Option<String>,
    deletion: Option<String>,
) -> Result<SyncSelectionDto, String> {
    use sync::selection::{AudioPolicy, DeletionPolicy, Direction};

    let db = state.db().map_err(err_str)?;
    let mut sel = sync_selection(&db);

    if let Some(d) = direction {
        sel.direction = Direction::parse(&d)
            .ok_or_else(|| format!("方向取值无效:{d}(可选 both | upload | download)"))?;
    }
    if let Some(a) = audio {
        sel.audio = AudioPolicy::parse(&a)
            .ok_or_else(|| format!("录音取值无效:{a}(可选 sync | skip)"))?;
    }
    if let Some(d) = deletion {
        sel.deletion = DeletionPolicy::parse(&d)
            .ok_or_else(|| format!("删除策略取值无效:{d}(可选 keep | text | mirror)"))?;
    }

    save_sync_selection(&db, &sel).map_err(err_str)?;
    Ok(sel.into())
}

/// 勾选/取消勾选一个文件或文件夹。
///
/// **取消勾选只是"不同步",不会删云端。** 界面文案也要说清这点。
#[tauri::command]
fn set_sync_excluded(
    state: State<AppState>,
    path: String,
    excluded: bool,
) -> Result<SyncSelectionDto, String> {
    let db = state.db().map_err(err_str)?;
    let mut sel = sync_selection(&db);
    if excluded {
        sel.exclude_path(&path);
    } else {
        sel.include_path(&path);
    }
    save_sync_selection(&db, &sel).map_err(err_str)?;
    Ok(sel.into())
}

/// 批量设置(给"整个工程全不选"之类的操作用)。
#[tauri::command]
fn set_sync_excluded_many(
    state: State<AppState>,
    paths: Vec<String>,
    excluded: bool,
) -> Result<SyncSelectionDto, String> {
    let db = state.db().map_err(err_str)?;
    let mut sel = sync_selection(&db);
    for p in paths {
        if excluded {
            sel.exclude_path(&p);
        } else {
            sel.include_path(&p);
        }
    }
    save_sync_selection(&db, &sel).map_err(err_str)?;
    Ok(sel.into())
}

/// 一键"全部同步":清空全部逐项排除。
#[tauri::command]
fn clear_sync_excludes(state: State<AppState>) -> Result<SyncSelectionDto, String> {
    let db = state.db().map_err(err_str)?;
    let mut sel = sync_selection(&db);
    sel.user_excludes.clear();
    save_sync_selection(&db, &sel).map_err(err_str)?;
    Ok(sel.into())
}

/// 同步清单树。
///
/// `probe` 为 true 时会联网为每个文件发 HEAD 请求确认远端状态 ——
/// 列云端目录树(带大小与修改时间)。
///
/// `rel_dir` 空 = 远端根。`depth` 限制递归层数 —— 界面首屏只拉一两层,
/// 展开某个目录时再拉它的下一层。
#[tauri::command]
async fn cloud_tree(
    state: State<'_, AppState>,
    rel_dir: Option<String>,
    depth: Option<usize>,
) -> Result<Vec<sync::remote::RemoteNode>, String> {
    let db = state.db().map_err(err_str)?;
    let cfg = webdav_config(&db).map_err(err_str)?;
    if cfg.username.is_empty() || cfg.password.is_empty() {
        return Err("还没配置 WebDAV 账号。请先在「云端同步」里填好账号密码。".into());
    }
    let dir = rel_dir.unwrap_or_default();
    // 上限 4 层:工程结构是 工程/分组/文件,再多说明远端结构异常
    let d = depth.unwrap_or(2).clamp(1, sync::remote::MAX_TREE_DEPTH);

    let client = sync::WebDavClient::new(cfg).map_err(err_str)?;
    sync::remote::fetch_tree(&client, &dir, d)
        .await
        .map_err(err_str)
}

/// 本地 ↔ 云端 差异对照。
///
/// 返回的树里每个目录都带 `attention`(需要关注的文件数)与
/// `differing` / `local_only` / `remote_only` 三个分类计数,
/// 界面据此决定展开哪一支默认高亮。
#[tauri::command]
async fn cloud_diff(
    state: State<'_, AppState>,
    depth: Option<usize>,
) -> Result<Vec<sync::remote::DiffNode>, String> {
    let db = state.db().map_err(err_str)?;
    let files = state.files().map_err(err_str)?;
    let cfg = webdav_config(&db).map_err(err_str)?;
    if cfg.username.is_empty() || cfg.password.is_empty() {
        return Err("还没配置 WebDAV 账号。请先在「云端同步」里填好账号密码。".into());
    }
    let d = depth.unwrap_or(3).clamp(1, sync::remote::MAX_TREE_DEPTH);

    // 本地文件表:rel_path → (大小, 内容哈希)
    let local = local_file_index(&files).map_err(err_str)?;

    let client = sync::WebDavClient::new(cfg).map_err(err_str)?;
    sync::remote::fetch_diff(&client, &local, d)
        .await
        .map_err(err_str)
}

/// 遍历本地存储目录,产出 `rel_path → (size, hash)`。
///
/// # 只收"同步引擎会传的文件"
///
/// 不是简单地全盘遍历 —— store 根下还有两个**本机专用**文件:
///
/// - `device.id`      本机标识,同步它会让两台机器互相覆盖
/// - `sync-state.json` 本机同步状态,设计上就不同步
///
/// 它们永远不会出现在云端。若算进对照里,界面会一直显示"仅本地 2",
/// 看起来像有问题,其实是正常的。同步引擎那边的
/// [`rs_core::store::files::FileStore::list_sync_files`] 只遍历
/// `transcript`/`summary`/`speaker_labels` 三个子目录,天然避开了它们 ——
/// 这里保持一致。
///
/// `manifest.json` 是要同步的(它在远端存在),所以放在白名单里。
fn local_file_index(
    files: &rs_core::store::files::FileStore,
) -> anyhow::Result<std::collections::BTreeMap<String, (u64, Option<String>)>> {
    let root = files.root();
    let mut out = std::collections::BTreeMap::new();

    // 与同步引擎一致:三个内容目录 + 工程目录 + manifest
    let include_audio = true; // 对照时音频也要看 —— 用户想知道它传上去没有
    let mut targets: Vec<std::path::PathBuf> = Vec::new();

    for sub in ["transcript", "summary", "speaker_labels"] {
        let p = root.join(sub);
        if !p.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&p).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                targets.push(entry.path().to_path_buf());
            }
        }
    }

    let projects = root.join("projects");
    if projects.is_dir() {
        for entry in walkdir::WalkDir::new(&projects)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if !include_audio
                && entry
                    .path()
                    .components()
                    .any(|c| c.as_os_str() == rs_core::project::AUDIO_DIR)
            {
                continue;
            }
            targets.push(entry.path().to_path_buf());
        }
    }

    // manifest 在远端确实有,算进来
    let mf = files.manifest_path();
    if mf.is_file() {
        targets.push(mf);
    }

    for p in targets {
        let name = p.file_name().map(|s| s.to_string_lossy().to_string());
        // 跳过临时文件
        if let Some(n) = &name {
            if n.ends_with(".tmp") || n.ends_with(".part") || n.starts_with('.') {
                continue;
            }
        }
        let Ok(rel) = p.strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        out.insert(rel, (size, None));
    }

    Ok(out)
}

/// 删除云端的文件或目录(递归)。
///
/// 这是**用户主动发起**的操作,不写墓碑、不查删除策略 —— 见
/// [`sync::remote`] 的模块文档。界面上必须先让用户确认。
#[tauri::command]
async fn cloud_delete(
    state: State<'_, AppState>,
    rel_path: String,
    is_dir: bool,
) -> Result<sync::remote::DeleteReport, String> {
    let db = state.db().map_err(err_str)?;
    let cfg = webdav_config(&db).map_err(err_str)?;
    if cfg.username.is_empty() || cfg.password.is_empty() {
        return Err("还没配置 WebDAV 账号。".into());
    }
    let node = sync::remote::RemoteNode {
        name: rel_path.rsplit('/').next().unwrap_or(&rel_path).to_string(),
        rel_path: rel_path.clone(),
        is_dir,
        size: None,
        modified: None,
        etag: None,
        children: Vec::new(),
    };
    let client = sync::WebDavClient::new(cfg).map_err(err_str)?;
    sync::remote::delete_node(&client, &node)
        .await
        .map_err(err_str)
}

/// 单个文件的同步方向。
#[derive(serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum OneWay {
    /// 本地 → 云端
    Upload,
    /// 云端 → 本地
    Download,
}

/// 单独同步**一个文件**(不走同步引擎、不写清单)。
///
/// # 与 `run_sync` 的分工
///
/// `run_sync` 按规则整批跑;这个针对用户在对照视图里挑中的某一个文件。
/// 典型场景:看到某个文件"大小不同",想立刻把本地这版推上去。
///
/// # 为什么不写 manifest / sync-state
///
/// 保持"手动"与"自动"两条路径分离。手动传完的文件在下一次 `run_sync` 时
/// 会因为 manifest 里没有记录而被判定为"需要上传" —— 那时再传一次即可,
/// 内容一致所以代价只是多一次请求。
///
/// 反过来(手动操作去改 manifest)危险得多:manifest 是跨设备的合并依据,
/// 手写它可能让别的设备误判。
#[tauri::command]
async fn cloud_sync_one(
    state: State<'_, AppState>,
    rel_path: String,
    direction: OneWay,
) -> Result<String, String> {
    let db = state.db().map_err(err_str)?;
    let files = state.files().map_err(err_str)?;
    let cfg = webdav_config(&db).map_err(err_str)?;
    if cfg.username.is_empty() || cfg.password.is_empty() {
        return Err("还没配置 WebDAV 账号。".into());
    }
    let root = files.root().to_path_buf();
    let client = sync::WebDavClient::new(cfg).map_err(err_str)?;

    match direction {
        OneWay::Upload => {
            // 防目录穿越:rel_path 来自界面,不该含 .. 或绝对路径
            let abs = safe_join(&root, &rel_path)?;
            if !abs.is_file() {
                return Err(format!("本地文件不存在:{}", abs.display()));
            }
            let n = sync::remote::upload_one(&client, &abs, &rel_path)
                .await
                .map_err(err_str)?;
            Ok(format!("已上传 {rel_path}({n} 字节)"))
        }
        OneWay::Download => {
            let abs = safe_join(&root, &rel_path)?;
            match sync::remote::download_one(&client, &rel_path, &abs)
                .await
                .map_err(err_str)?
            {
                Some(n) => Ok(format!("已下载 {rel_path}({n} 字节)")),
                None => Err(format!("云端没有这个文件:{rel_path}")),
            }
        }
    }
}

/// 把相对路径安全地拼到 root 下。
///
/// 拒绝绝对路径与 `..` —— 这个路径来自界面,虽然是自己人用,
/// 但"绝不信任来自前端的路径"是条便宜的规矩。
fn safe_join(root: &std::path::Path, rel: &str) -> Result<std::path::PathBuf, String> {
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        return Err("路径为空".into());
    }
    let p = std::path::Path::new(rel);
    if p.is_absolute() {
        return Err(format!("不接受绝对路径:{rel}"));
    }
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                return Err(format!("路径不能包含 ..:{rel}"));
            }
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                return Err(format!("路径不合法:{rel}"));
            }
            _ => {}
        }
    }
    Ok(root.join(p))
}

/// 准确但慢(上百个文件可能十几秒)。界面首屏传 false 秒开,
/// 用户点「核实云端状态」时再传 true。
#[tauri::command]
async fn get_sync_inventory(
    state: State<'_, AppState>,
    probe: bool,
) -> Result<sync::inventory::InventoryNode, String> {
    let db = state.db().map_err(err_str)?;
    let files = state.files().map_err(err_str)?;
    let cfg = webdav_config(&db).map_err(err_str)?;
    let sel = sync_selection(&db);

    tauri::async_runtime::spawn_blocking(move || -> anyhow::Result<_> {
        // 没配账号时无法联网,直接用本地状态推断
        let can_probe = probe && !cfg.username.is_empty() && !cfg.password.is_empty();
        let s = if can_probe {
            sync::Syncer::new(cfg, &files)?.with_selection(sel.clone())
        } else {
            sync::Syncer::new_offline(&files)?.with_selection(sel.clone())
        };
        s.inventory(&sel, can_probe)
    })
    .await
    .map_err(|e| format!("任务失败: {e}"))?
    .map_err(err_str)
}

/// 检查前端依赖是否就位。
///
/// 目前只有一项:mermaid(思维导图渲染库)。它体积大(5.4MB)所以不进版本库,
/// 由 `ui/vendor/README.txt` 的步骤下载。缺了不影响其他功能,只影响导图显示 ——
/// 但界面上必须说清楚,否则用户只会看到一片空白。
///
/// ⚠️ **不能用裸相对路径** —— 双击 exe 启动时工作目录不是项目根,
/// 于是明明文件在,却报"未找到"。这与 `models/` 的坑是同一个。
#[tauri::command]
fn check_frontend_deps(state: State<AppState>) -> FrontendDeps {
    let files = state.files().ok();
    let data_dir = state.data_dir.clone();

    // 与 models/binaries 用同一套搜索顺序
    let found = rs_core::paths::find_asset_file("ui/vendor/mermaid.min.js", &data_dir)
        // 打包后前端资源可能被嵌进 exe,此时文件系统里没有 —— 但界面能跑到这里
        // 说明资源已经加载成功,所以也算"就位"
        .or_else(|| {
            files
                .as_ref()
                .and_then(|_| rs_core::paths::find_asset_file("mermaid.min.js", &data_dir))
        });

    match found {
        Some(p) => FrontendDeps {
            mermaid_present: true,
            mermaid_size_kb: std::fs::metadata(&p).map(|m| m.len() / 1024).unwrap_or(0),
            mermaid_path: p.to_string_lossy().to_string(),
        },
        None => FrontendDeps {
            mermaid_present: false,
            mermaid_size_kb: 0,
            mermaid_path: "ui/vendor/mermaid.min.js".to_string(),
        },
    }
}

#[derive(Serialize)]
struct FrontendDeps {
    mermaid_present: bool,
    mermaid_size_kb: u64,
    mermaid_path: String,
}

// ---------------------------------------------------------------------------
// 命令:声纹档案
// ---------------------------------------------------------------------------

#[tauri::command]
fn list_profiles(state: State<AppState>) -> Result<Vec<ProfileRow>, String> {
    let db = state.db().map_err(err_str)?;
    let files = state.files().map_err(err_str)?;
    let vs = VoiceprintStore::new(&db, &files);
    let list = vs.list_profiles().map_err(err_str)?;
    Ok(list
        .into_iter()
        .map(|p| {
            let (n, ms) = vs.sample_stats(&p.profile_id).unwrap_or((0, 0));
            ProfileRow {
                id: p.profile_id,
                name: p.display_name,
                sample_count: n,
                total_minutes: ms / 60_000,
                model: p.embedding_model,
            }
        })
        .collect())
}

#[tauri::command]
fn delete_profile(state: State<AppState>, profile_id: String) -> Result<(), String> {
    let db = state.db().map_err(err_str)?;
    let files = state.files().map_err(err_str)?;
    let vs = VoiceprintStore::new(&db, &files);
    vs.delete_profile(&profile_id).map_err(err_str)
}

#[tauri::command]
fn rename_profile(state: State<AppState>, profile_id: String, name: String) -> Result<(), String> {
    let db = state.db().map_err(err_str)?;
    let files = state.files().map_err(err_str)?;
    let vs = VoiceprintStore::new(&db, &files);
    vs.rename_profile(&profile_id, &name).map_err(err_str)
}

// ---------------------------------------------------------------------------
// 命令:工程
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ProjectCard {
    id: String,
    short_id: String,
    title: String,
    slug: String,
    dir: String,
    created_at: i64,
    updated_at: i64,
    duration_text: String,
    scene_label: Option<String>,
    speaker_count: u32,
    /// 产物是否齐全;缺什么用 missing 列
    complete: bool,
    missing: Vec<String>,
    has_mindmap: bool,
}

#[derive(Serialize)]
struct ProjectDetail {
    id: String,
    title: String,
    dir: String,
    duration_text: String,
    created_at: i64,
    scene_label: Option<String>,
    scene_confidence: Option<f32>,
    asr_model: Option<String>,
    asr_backend: Option<String>,
    speaker_count: u32,
    complete: bool,
    missing: Vec<String>,
    /// 各产物内容(未生成的为 None)
    transcript: Option<String>,
    srt: Option<String>,
    summary_detailed: Option<String>,
    summary_brief: Option<String>,
    /// Mermaid 源码
    mindmap: Option<String>,
    /// 文本大纲(图渲染失败时的降级)
    outline: Option<String>,
    /// 导图语法自检是否通过
    mindmap_syntax_ok: bool,
    /// 自检发现/修复的问题
    mindmap_issues: Vec<String>,
}

fn project_to_card(p: &rs_core::project::Project) -> ProjectCard {
    let a = &p.meta.artifacts;
    ProjectCard {
        short_id: p.meta.id.chars().take(8).collect(),
        id: p.meta.id.clone(),
        title: p.meta.title.clone(),
        slug: p.meta.slug.clone(),
        dir: p.dir.to_string_lossy().to_string(),
        created_at: p.meta.created_at,
        updated_at: p.meta.updated_at,
        duration_text: format_ms(p.meta.duration_ms),
        scene_label: p.meta.scene.map(|s| s.label().to_string()),
        speaker_count: p.meta.speaker_count,
        complete: a.missing().is_empty(),
        missing: a.missing().iter().map(|s| s.to_string()).collect(),
        has_mindmap: a.mindmap,
    }
}

#[tauri::command]
fn list_projects(state: State<AppState>) -> Result<Vec<ProjectCard>, String> {
    use rs_core::project::ProjectStore;
    let files = state.files().map_err(err_str)?;
    let store = ProjectStore::new(files.root());
    let list = store.list().map_err(err_str)?;
    Ok(list.iter().map(project_to_card).collect())
}

#[tauri::command]
fn get_project(state: State<AppState>, id: String) -> Result<ProjectDetail, String> {
    use rs_core::project::{ProjectStore, SummaryKind};
    let files = state.files().map_err(err_str)?;
    let store = ProjectStore::new(files.root());
    let p = store
        .find(&id)
        .map_err(err_str)?
        .ok_or_else(|| format!("找不到工程: {id}"))?;

    let mindmap_src = p.read_mindmap();
    // 对读回来的 Mermaid 再做一次自检 —— 文件可能被手工改过
    let (syntax_ok, issues) = match &mindmap_src {
        Some(s) => rs_core::types::check_mermaid(s),
        None => (true, vec![]),
    };

    Ok(ProjectDetail {
        id: p.meta.id.clone(),
        title: p.meta.title.clone(),
        dir: p.dir.to_string_lossy().to_string(),
        duration_text: format_ms(p.meta.duration_ms),
        created_at: p.meta.created_at,
        scene_label: p.meta.scene.map(|s| s.label().to_string()),
        scene_confidence: p.meta.scene_confidence,
        asr_model: p.meta.asr_model.clone(),
        asr_backend: p.meta.asr_backend.clone(),
        speaker_count: p.meta.speaker_count,
        complete: p.meta.artifacts.missing().is_empty(),
        missing: p
            .meta
            .artifacts
            .missing()
            .iter()
            .map(|s| s.to_string())
            .collect(),
        transcript: p.read_transcript_md(),
        srt: p.read_srt(),
        summary_detailed: p.read_summary(SummaryKind::Detailed),
        summary_brief: p.read_summary(SummaryKind::Brief),
        mindmap: mindmap_src,
        outline: std::fs::read_to_string(p.dir.join("mindmap-outline.md")).ok(),
        mindmap_syntax_ok: syntax_ok,
        mindmap_issues: issues,
    })
}

/// 列出工程里的文件(给"打开目录"与文件列表用)。
#[tauri::command]
fn list_project_files(state: State<AppState>, id: String) -> Result<Vec<ProjectFile>, String> {
    use rs_core::project::ProjectStore;
    let files = state.files().map_err(err_str)?;
    let store = ProjectStore::new(files.root());
    let p = store
        .find(&id)
        .map_err(err_str)?
        .ok_or_else(|| format!("找不到工程: {id}"))?;

    let mut out = Vec::new();
    collect_files(&p.dir, &p.dir, &mut out).map_err(err_str)?;
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

#[derive(Serialize)]
struct ProjectFile {
    rel: String,
    abs: String,
    size: u64,
    is_audio: bool,
}

fn collect_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<ProjectFile>,
) -> anyhow::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let path = e.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let is_audio = matches!(
                path.extension().and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase()),
                Some(ref x) if matches!(x.as_str(), "wav" | "mp3" | "m4a" | "aac" | "flac" | "ogg" | "opus")
            );
            out.push(ProjectFile {
                rel,
                abs: path.to_string_lossy().to_string(),
                size: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                is_audio,
            });
        }
    }
    Ok(())
}

/// 打开工程目录(用系统文件管理器)。
#[tauri::command]
fn open_project_dir(state: State<AppState>, id: String) -> Result<String, String> {
    use rs_core::project::ProjectStore;
    let files = state.files().map_err(err_str)?;
    let store = ProjectStore::new(files.root());
    let p = store
        .find(&id)
        .map_err(err_str)?
        .ok_or_else(|| format!("找不到工程: {id}"))?;
    // 交给前端用 opener 插件打开
    Ok(p.dir.to_string_lossy().to_string())
}

/// 工程重命名的结果。前端据此提示"云端路径已变"。
#[derive(serde::Serialize)]
struct RenameProjectResult {
    /// 改名后的目录
    dir: String,
    /// 原标题
    old_title: String,
    /// 新标题
    new_title: String,
    /// 目录名是否真的变了(= 云端路径变了)
    slug_changed: bool,
    /// 旧目录名
    old_slug: String,
    /// 会话表标题没同步成功时的说明(工程本身已改好)
    title_sync_warning: Option<String>,
}

/// 重命名工程:标题与目录名一起改。
///
/// **不改工程 ID** —— ID 是音频内容哈希,与名字无关。所以改名之后
/// 转写缓存、声纹标签、同步清单里按 ID 索引的东西全部照旧。
#[tauri::command]
fn rename_project(
    state: State<AppState>,
    id: String,
    title: String,
) -> Result<RenameProjectResult, String> {
    use rs_core::project::ProjectStore;
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err("新标题不能为空".into());
    }
    let files = state.files().map_err(err_str)?;
    let store = ProjectStore::new(files.root());
    let old_title = store
        .find(&id)
        .map_err(err_str)?
        .ok_or_else(|| format!("找不到工程: {id}"))?
        .meta
        .title
        .clone();

    // ★ 用共用的那份实现 —— 它会一并同步会话表标题。
    //
    //   标题在两处各存一份(project.json 与 sessions.title),
    //   靠音频内容哈希关联但是两套独立存储。只改一处的话,
    //   同一个录音在「我的工程」和「历史记录」会显示不同的名字。
    let db = state.db().map_err(err_str)?;
    let out = rs_core::project::rename_project_synced(&store, &db, &id, &title)
        .map_err(err_str)?;

    Ok(RenameProjectResult {
        dir: out.dir.to_string_lossy().to_string(),
        old_title,
        new_title: title,
        slug_changed: out.slug_changed,
        old_slug: out.old_slug,
        title_sync_warning: out.warning,
    })
}

/// 删除工程的结果,给界面做结果提示用。
#[derive(serde::Serialize)]
struct DeleteProjectResult {
    id: String,
    title: String,
    bytes_freed: u64,
    files_deleted: usize,
    /// 一并删掉的按哈希命名的产物文件数
    content_files_deleted: usize,
    /// **没删**的产物文件数(有别的工程共享同一个录音时为非零)
    content_files_kept: usize,
    /// 云端是否可能还有一份 —— 界面据此提醒去「云端管理」清理
    maybe_in_cloud: bool,
    source_audio: Option<String>,
    source_deleted: bool,
}

/// 删除工程。
///
/// 删工程目录(含音频副本)与按哈希命名的产物文件。
/// **历史记录与云端副本不动** —— 见 [`rs_core::project::delete_project`]。
#[tauri::command]
fn delete_project(
    state: State<AppState>,
    id: String,
    delete_source: Option<bool>,
) -> Result<DeleteProjectResult, String> {
    use rs_core::project::ProjectStore;
    let files = state.files().map_err(err_str)?;
    let db = state.db().map_err(err_str)?;
    let store = ProjectStore::new(files.root());

    let manifest = sync::manifest::Manifest::load_or_new(&files.manifest_path())
        .map_err(err_str)?;

    // 先按 ID 前缀解析出**目录** —— 同一个音频可以建多个工程,
    // 那时 ID 不唯一,只有目录能唯一定位。
    let p = store
        .find(&id)
        .map_err(err_str)?
        .ok_or_else(|| format!("找不到工程: {id}"))?;

    let out = rs_core::project::delete_project(
        &store,
        &files,
        &db,
        &p.dir,
        delete_source.unwrap_or(false),
    )
    .map_err(err_str)?;

    let maybe_in_cloud = out.had_cloud_copy(&manifest);

    Ok(DeleteProjectResult {
        id: out.id,
        title: out.title,
        bytes_freed: out.bytes_freed,
        files_deleted: out.files_deleted,
        content_files_deleted: out.content_files_deleted.len(),
        content_files_kept: out.content_files_kept.len(),
        maybe_in_cloud,
        source_audio: out.source_audio.map(|p| p.to_string_lossy().to_string()),
        source_deleted: out.source_deleted,
    })
}

/// 把导图源码写盘(PNG 由前端截图后回传,见 save_png)。
#[tauri::command]
fn save_mermaid_source(state: State<AppState>, id: String, source: String) -> Result<String, String> {
    use rs_core::project::{ProjectStore, MINDMAP};
    let files = state.files().map_err(err_str)?;
    let store = ProjectStore::new(files.root());
    let p = store
        .find(&id)
        .map_err(err_str)?
        .ok_or_else(|| format!("找不到工程: {id}"))?;
    // 用户手改过导图时用这个接口存回去
    let (ok, issues) = rs_core::types::check_mermaid(&source);
    if !ok {
        return Err(format!("语法自检未通过:{}\n请修正后再保存。", issues.join("、")));
    }
    let path = p.dir.join(MINDMAP);
    std::fs::write(&path, &source).map_err(|e| format!("写入失败: {e}"))?;
    // 同步更新文本大纲
    let outline = rs_core::llm::prompt::mermaid_to_outline(&source);
    let _ = std::fs::write(p.dir.join("mindmap-outline.md"), outline);
    Ok(path.to_string_lossy().to_string())
}

/// 保存前端渲染出的 PNG(data URL)。
#[tauri::command]
fn save_png(path: String, data_url: String) -> Result<String, String> {
    let b64 = data_url
        .split_once(',')
        .map(|(_, b)| b)
        .unwrap_or(&data_url);
    let bytes = base64_decode::decode(b64).map_err(|e| format!("PNG 数据解码失败: {e}"))?;
    std::fs::write(&path, &bytes).map_err(|e| format!("写入失败: {e}"))?;
    Ok(path)
}

/// 极简 base64 解码(不引额外依赖)。
///
/// 只用于"前端 canvas 导出的 PNG data URL 写盘"这一个场景,
/// 所以不需要支持流式或 URL-safe 变体。
mod base64_decode {
    pub fn decode(s: &str) -> Result<Vec<u8>, String> {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut lut = [255u8; 256];
        for (i, c) in TABLE.iter().enumerate() {
            lut[*c as usize] = i as u8;
        }
        let clean: Vec<u8> = s
            .bytes()
            .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
            .collect();
        let mut out = Vec::with_capacity(clean.len() * 3 / 4);
        for chunk in clean.chunks(4) {
            if chunk.len() < 2 {
                break;
            }
            let mut buf = [0u8; 4];
            for (i, b) in chunk.iter().enumerate() {
                let v = lut[*b as usize];
                if v == 255 {
                    return Err(format!("非法 base64 字符: {}", *b as char));
                }
                buf[i] = v;
            }
            out.push((buf[0] << 2) | (buf[1] >> 4));
            if chunk.len() > 2 {
                out.push((buf[1] << 4) | (buf[2] >> 2));
            }
            if chunk.len() > 3 {
                out.push((buf[2] << 6) | buf[3]);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::{base64_decode, safe_join};

    // --- 路径安全 ----------------------------------------------------------
    //
    // `rel_path` 来自界面。虽然是自用软件,但"绝不信任来自前端的路径"
    // 是一条几乎零成本的规矩 —— 一旦哪天界面出了 bug 或被注入,
    // 单文件同步/删除就可能写到 store 之外。

    #[test]
    fn safe_join_accepts_normal_relative_paths() {
        let root = std::path::Path::new("D:\\store");
        assert_eq!(
            safe_join(root, "projects/P/transcript.md").unwrap(),
            root.join("projects/P/transcript.md")
        );
        // 前导斜杠会被去掉,不当作绝对路径
        assert_eq!(
            safe_join(root, "/projects/P/x.md").unwrap(),
            root.join("projects/P/x.md")
        );
        // 中文与空格
        assert_eq!(
            safe_join(root, "projects/2026-09-11_9月11日 计算机视觉/a.md").unwrap(),
            root.join("projects/2026-09-11_9月11日 计算机视觉/a.md")
        );
    }

    #[test]
    fn safe_join_rejects_parent_dir_traversal() {
        let root = std::path::Path::new("D:\\store");
        // ★ 核心防线:不能借 .. 爬到 store 外面
        for bad in [
            "../secret.txt",
            "projects/../../secret.txt",
            "a/../../b",
            "..",
        ] {
            assert!(
                safe_join(root, bad).is_err(),
                "含 .. 的路径必须被拒: {bad}"
            );
        }
    }

    #[test]
    fn safe_join_rejects_absolute_paths() {
        let root = std::path::Path::new("D:\\store");
        for bad in [
            "C:\\Windows\\System32\\x.dll",
            "D:\\other\\file.txt",
        ] {
            // Windows 上绝对路径有盘符前缀;在别的平台这条可能退化成
            // 普通相对路径,所以只断言"要么拒绝、要么确实落在 root 下"
            match safe_join(root, bad) {
                Err(_) => {}
                Ok(p) => assert!(
                    p.starts_with(root),
                    "绝对路径不该逃出 root: {p:?}"
                ),
            }
        }
    }

    #[test]
    fn safe_join_rejects_empty() {
        let root = std::path::Path::new("D:\\store");
        assert!(safe_join(root, "").is_err());
        assert!(safe_join(root, "/").is_err());
    }

    #[test]
    fn base64_decodes_png_signature() {
        // PNG 魔数 89 50 4E 47 0D 0A 1A 0A 的 base64
        let got = base64_decode::decode("iVBORw0KGgo=").unwrap();
        assert_eq!(got, vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
    }

    #[test]
    fn base64_handles_padding_variants() {
        // 1/2/3 字节输入对应 2/3/4 个输出
        assert_eq!(base64_decode::decode("QQ==").unwrap(), b"A");
        assert_eq!(base64_decode::decode("QUI=").unwrap(), b"AB");
        assert_eq!(base64_decode::decode("QUJD").unwrap(), b"ABC");
    }

    #[test]
    fn base64_ignores_whitespace_and_newlines() {
        // canvas.toDataURL 输出通常没有换行,但手工粘贴的可能有
        let a = base64_decode::decode("QUJD").unwrap();
        let b = base64_decode::decode("QU\nJD\n").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn base64_roundtrip_random_bytes() {
        // 用已知向量覆盖所有 256 种字节值,验证解码正确
        let bytes: Vec<u8> = (0..=255u8).collect();
        let mut encoded = String::new();
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            encoded.push(TABLE[(b[0] >> 2) as usize] as char);
            encoded.push(TABLE[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
            if chunk.len() > 1 {
                encoded.push(TABLE[(((b[1] & 0x0F) << 2) | (b[2] >> 6)) as usize] as char);
            } else {
                encoded.push('=');
            }
            if chunk.len() > 2 {
                encoded.push(TABLE[(b[2] & 0x3F) as usize] as char);
            } else {
                encoded.push('=');
            }
        }
        let back = base64_decode::decode(&encoded).unwrap();
        assert_eq!(back, bytes, "全字节范围往返应一致");
    }

    #[test]
    fn base64_rejects_illegal_chars() {
        assert!(base64_decode::decode("QQ*Q").is_err());
    }

    #[test]
    fn base64_empty_input() {
        assert_eq!(base64_decode::decode("").unwrap(), Vec::<u8>::new());
    }
}


// ---------------------------------------------------------------------------
// 命令:统计
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct StatsInfo {
    sessions: i64,
    transcript_chunks: i64,
    diarize_results: i64,
    enrollment_samples: i64,
    sync_files: usize,
    sync_bytes: u64,
}

#[tauri::command]
fn get_stats(state: State<AppState>) -> Result<StatsInfo, String> {
    let db = state.db().map_err(err_str)?;
    let files = state.files().map_err(err_str)?;
    let s = cache::stats(&db).map_err(err_str)?;
    let list = files.list_sync_files().map_err(err_str)?;
    let bytes: u64 = list
        .iter()
        .filter_map(|f| std::fs::metadata(f).ok())
        .map(|m| m.len())
        .sum();
    Ok(StatsInfo {
        sessions: s.sessions,
        transcript_chunks: s.transcript_chunks,
        diarize_results: s.diarize_results,
        enrollment_samples: s.enrollment_samples,
        sync_files: list.len(),
        sync_bytes: bytes,
    })
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let data_dir = std::env::var_os("RECSUM_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(rs_core::default_data_dir);

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            data_dir,
            cancel: Mutex::new(None),
        })
        .setup(|app| {
            // `--diag`:在页面里打开运行时诊断浮层,便于没有 devtools 时排查前端问题
            if std::env::args().any(|a| a == "--diag") {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.eval("window.__DIAG__ = true;");
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            probe_hardware,
            list_models,
            list_sessions,
            get_session,
            rename_speaker,
            render_view,
            save_text,
            start_run,
            cancel_run,
            is_running,
            get_llm_config,
            set_llm_config,
            clear_api_key,
            test_llm,
            get_sync_config,
            set_sync_config,
            test_sync,
            sync_plan,
            run_sync,
            audio_sync_hint,
            get_sync_selection,
            set_sync_selection,
            set_sync_excluded,
            set_sync_excluded_many,
            clear_sync_excludes,
            get_sync_inventory,
            cloud_tree,
            cloud_diff,
            cloud_delete,
            cloud_sync_one,
            check_frontend_deps,
            list_profiles,
            delete_profile,
            rename_profile,
            list_projects,
            get_project,
            list_project_files,
            open_project_dir,
            rename_project,
            delete_project,
            save_mermaid_source,
            save_png,
            get_stats,
        ])
        .run(tauri::generate_context!())
        .expect("启动 Tauri 应用失败");
}
