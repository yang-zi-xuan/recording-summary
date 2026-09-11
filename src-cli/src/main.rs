//! `rs` —— 录音转总结命令行入口。
//!
//! 先用 CLI 把管线跑通,GUI 只是后加的进度条(见技术方案 §2.1)。
//!
//! 典型用法:
//! ```text
//! rs probe                       # 看硬件探测结果与推荐模型
//! rs run 录音.mp3                # 全自动:转写 → 说话人 → 场景 → 纪要
//! rs run 录音.mp3 --no-diarize   # 跳过说话人区分
//! rs show <会话ID> --view dialogue
//! rs rename <会话ID> 0 张老师
//! rs list
//! ```

mod progress;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use keyring;
use rs_core::asr::WhisperCppSidecar;
use rs_core::hardware::{self, SidecarLocator};
use rs_core::llm::{self, BlockingSummarizer, LlmConfig, PriceTable};
use rs_core::pipeline::view::ViewKind;
use rs_core::pipeline::{Pipeline, PipelineConfig};
use rs_core::store::cache;
use rs_core::store::files::FileStore;
use rs_core::store::Db;
use rs_core::types::{
    AudioRef, AudioSourceKind, BackendPref, DiarizeMode, ModelTier, SpeakerLabels,
};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "rs",
    version,
    about = "录音转总结 —— 本地转写 + 云端纪要",
    long_about = "把课堂/会议录音变成带时间戳、带发言人的结构化纪要。\n\
                  转写在本地完成(音频不出机器),只有文字会发送给所选 LLM 服务商。"
)]
struct Cli {
    /// 数据目录(默认在用户数据目录)
    #[arg(long, global = true, env = "RECSUM_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// sidecar 二进制目录(其下按后端分 cpu/ cuda/ vulkan/)
    #[arg(long, global = true, env = "RECSUM_BINARIES")]
    binaries: Option<PathBuf>,

    /// 模型目录
    #[arg(long, global = true, env = "RECSUM_MODELS")]
    models: Option<PathBuf>,

    /// 详细日志
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 探测硬件,显示将使用的后端、推荐模型与预计速度
    Probe {
        /// 只输出 JSON(便于脚本处理)
        #[arg(long)]
        json: bool,
    },

    /// 处理一个音频文件:转写 → 说话人区分 → 场景判断 → 纪要
    Run {
        /// 输入音频/视频文件
        input: PathBuf,

        /// 强制指定后端(默认自动探测)
        #[arg(long, value_name = "cuda|vulkan|cpu|...")]
        backend: Option<String>,

        /// 指定模型档位(默认按硬件推荐)
        #[arg(long, value_name = "tiny|base|small|medium|large-v3-turbo")]
        model: Option<String>,

        /// 语言(默认自动检测)
        #[arg(long, value_name = "zh|en|auto")]
        language: Option<String>,

        /// 指定发言人数量(最准;不给则自动检测)
        #[arg(long, value_name = "N")]
        speakers: Option<u8>,

        /// 跳过说话人区分
        #[arg(long)]
        no_diarize: bool,

        /// 跳过 LLM 总结(只做转写)
        #[arg(long)]
        no_summary: bool,

        /// 术语表(可多次指定),用于 initial_prompt 与后续校正
        #[arg(long = "term", value_name = "词")]
        terms: Vec<String>,

        /// 分块目标时长(秒)
        #[arg(long, default_value_t = 300)]
        chunk_secs: u64,

        /// 处理完后立刻打印纪要
        #[arg(long)]
        print: bool,
    },

    /// 显示某个会话的转写
    Show {
        /// 会话 ID(或 ID 前缀)
        session: String,

        /// 视图:dialogue | timeline | plain | srt
        #[arg(long, default_value = "dialogue")]
        view: String,

        /// 同时打印纪要
        #[arg(long)]
        summary: bool,
    },

    /// 列出已处理的会话
    List {
        /// 最多显示条数
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },

    /// 工程(每次处理录音自动建立的目录)
    Projects {
        #[command(subcommand)]
        action: Option<ProjectAction>,
    },

    /// 修改发言人名字(只改标签,不重新转写)
    Rename {
        /// 会话 ID(或前缀)
        session: String,

        /// 说话人序号(从 0 开始)
        speaker_id: u32,

        /// 新名字
        name: String,
    },

    /// 列出/管理声纹档案
    Profiles {
        #[command(subcommand)]
        action: Option<ProfileAction>,
    },

    /// 云端同步(WebDAV)
    Sync {
        #[command(subcommand)]
        action: SyncAction,
    },

    /// 配置(API Key 等)
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// 缓存信息
    Stats,
}

#[derive(Subcommand, Debug)]
enum SyncAction {
    /// 设置 WebDAV 账号(密码存入 Windows 凭据管理器)
    Login {
        /// 用户名。Seafile 系格式:<账号>@auth.local
        username: String,
        /// 服务地址
        #[arg(long, default_value = "https://cloud.example.com/seafdav")]
        url: String,
        /// 远端子目录
        #[arg(long, default_value = "recording-summary")]
        dir: String,
    },
    /// 显示当前同步配置
    Show {
        /// 以 `KEY=VALUE` 形式输出,便于喂给诊断程序。
        ///
        /// ⚠️ **会打印明文密码。** 只在你明确要看的时候用。
        #[arg(long)]
        env: bool,
    },
    /// 测试连通性
    Test,
    /// 计算同步计划(不实际传输)
    Plan {
        /// 只同步匹配这些通配符的路径(可多次),例如 `projects/2026-09*`
        #[arg(long = "include", value_name = "PATTERN")]
        include: Vec<String>,
        /// 排除匹配这些通配符的路径(可多次),优先级高于 include
        #[arg(long = "exclude", value_name = "PATTERN")]
        exclude: Vec<String>,
        /// 只同步指定工程(可多次)。支持完整目录名、标题前缀、或 `YYYY-MM-DD`
        #[arg(long = "project", value_name = "DIR")]
        projects: Vec<String>,
        /// 录音是否参与:sync(默认) | skip
        #[arg(long, value_name = "MODE")]
        audio: Option<String>,
        /// 同步方向:both(默认) | upload(只上传) | download(只下载)
        #[arg(long, value_name = "MODE")]
        direction: Option<String>,
        /// 本地删了要不要删云端:keep(从不删) | text(默认,只删文本) | mirror(全删)
        #[arg(long, value_name = "MODE")]
        deletion: Option<String>,
        /// 不联网:假设远端为空。用于查看"哪些文件会被删"而不实际连接
        #[arg(long)]
        offline: bool,
    },
    /// 执行同步
    Run {
        /// 只同步匹配这些通配符的路径(可多次)
        #[arg(long = "include", value_name = "PATTERN")]
        include: Vec<String>,
        /// 排除匹配这些通配符的路径(可多次)
        #[arg(long = "exclude", value_name = "PATTERN")]
        exclude: Vec<String>,
        /// 只同步指定工程(可多次)
        #[arg(long = "project", value_name = "DIR")]
        projects: Vec<String>,
        /// 录音是否参与:sync(默认) | skip
        #[arg(long, value_name = "MODE")]
        audio: Option<String>,
        /// 同步方向:both(默认) | upload(只上传) | download(只下载)
        #[arg(long, value_name = "MODE")]
        direction: Option<String>,
        /// 本地删除:keep | text(默认) | mirror
        #[arg(long, value_name = "MODE")]
        deletion: Option<String>,
    },
    /// 从远端重建 manifest(manifest 丢失或损坏时用)
    Rebuild,
}

#[derive(Subcommand, Debug)]
enum ProfileAction {
    /// 列出全部档案
    List,
    /// 删除档案
    Delete { profile_id: String },
    /// 改名
    Rename { profile_id: String, name: String },
}

#[derive(Subcommand, Debug)]
enum ProjectAction {
    /// 列出全部工程(默认动作)
    List,
    /// 显示某个工程的详情与产物清单
    Show {
        /// 工程 ID(或前缀)
        id: String,
    },
    /// 打印工程里的某个产物
    Cat {
        /// 工程 ID(或前缀)
        id: String,
        /// 产物:brief | detailed | transcript | srt | mindmap | outline
        #[arg(default_value = "brief")]
        what: String,
    },
    /// 导出整个工程为 zip 风格的目录拷贝(便于发给别人)
    Export {
        /// 工程 ID(或前缀)
        id: String,
        /// 目标目录(不存在则创建)
        dest: PathBuf,
    },
    /// 重命名工程(标题与目录名一起改)
    Rename {
        /// 工程 ID(或前缀)
        id: String,
        /// 新标题
        title: String,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// 显示当前配置(Key 以掩码显示)
    Show,
    /// 保存 API Key 到系统凭据管理器
    SetKey { key: String },
    /// 删除已保存的 API Key
    ClearKey,
    /// 测试与 LLM 服务的连通性
    Test,
    /// 设置 Base URL(换供应商)
    SetBaseUrl { url: String },
    /// 设置模型名
    SetModel { model: String },

    /// 数据存到哪(工程、录音、数据库)
    DataDir {
        #[command(subcommand)]
        action: Option<DataDirAction>,
    },
}

#[derive(Subcommand, Debug)]
enum DataDirAction {
    /// 显示当前数据目录
    Show,
    /// 迁移到新目录(拷贝 → 更新指针 → 删旧目录),之后不再需要 --data-dir
    Move {
        /// 新目录,例如 D:\RecordingSummary
        path: PathBuf,
    },
    /// 回到系统默认位置(%APPDATA%),不迁移数据
    Reset,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum _Unused {}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();

    let level = if cli.verbose { "debug" } else { "warn" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .with_target(false)
        .init();

    let data_dir = cli.data_dir.clone().unwrap_or_else(rs_core::default_data_dir);
    // 提前抽出身后的路径参数,避免 match 移动 cli 后再借用
    let paths = resolve_paths(&cli, &data_dir);

    match cli.command {
        Command::Probe { json } => cmd_probe(&paths, json),
        Command::Run {
            input,
            backend,
            model,
            language,
            speakers,
            no_diarize,
            no_summary,
            terms,
            chunk_secs,
            print,
        } => {
            cmd_run(
                &paths,
                RunArgs {
                    input,
                    backend,
                    model,
                    language,
                    speakers,
                    no_diarize,
                    no_summary,
                    terms,
                    chunk_secs,
                    print,
                },
            )
        }
        Command::Show {
            session,
            view,
            summary,
        } => cmd_show(&data_dir, &session, &view, summary),
        Command::List { limit } => cmd_list(&data_dir, limit),
        Command::Rename {
            session,
            speaker_id,
            name,
        } => cmd_rename(&data_dir, &session, speaker_id, &name),
        Command::Profiles { action } => cmd_profiles(&data_dir, action),
        Command::Projects { action } => cmd_projects(&data_dir, action),
        Command::Sync { action } => cmd_sync(&data_dir, action),
        Command::Config { action } => cmd_config(&data_dir, action),
        Command::Stats => cmd_stats(&data_dir),
    }
}

// ---------------------------------------------------------------------------
// 路径解析
// ---------------------------------------------------------------------------

struct Paths {
    data_dir: PathBuf,
    binaries: PathBuf,
    models: PathBuf,
}

fn resolve_paths(cli: &Cli, data_dir: &std::path::Path) -> Paths {
    // 显式参数优先;否则按 rs_core::paths 的顺序搜索多个候选位置。
    //
    // 搜索而不是用裸的相对路径 —— 后者在工作目录不是项目根目录时会失效
    // (双击 exe、从别的目录调用都会)。
    Paths {
        data_dir: data_dir.to_path_buf(),
        binaries: cli
            .binaries
            .clone()
            .or_else(|| rs_core::paths::find_binaries_dir(data_dir))
            .unwrap_or_else(|| data_dir.join("binaries")),
        models: cli
            .models
            .clone()
            .or_else(|| rs_core::paths::find_models_dir(data_dir))
            .unwrap_or_else(|| data_dir.join("models")),
    }
}

// ---------------------------------------------------------------------------
// probe
// ---------------------------------------------------------------------------

fn cmd_probe(p: &Paths, json: bool) -> Result<()> {
    let locator = SidecarLocator::new(&p.binaries);
    let cores = hardware::cpu_cores();
    let vram = hardware::nvidia_vram_gb();

    let mut rows = Vec::new();
    for backend in rs_core::types::Backend::PRIORITY {
        let has_bin = locator.whisper_cli(backend).is_some();
        let avail = hardware::available(backend);
        let usable = has_bin && avail;
        rows.push((backend, has_bin, avail, usable));
    }

    let selected = hardware::select_backend(BackendPref::Auto, &locator);
    let effective = selected.unwrap_or(rs_core::types::Backend::Cpu);
    let model = hardware::recommend_model(effective, vram, cores);

    if json {
        let obj = serde_json::json!({
            "cpu_cores": cores,
            "vram_gb": vram,
            "nvidia_devices": hardware::nvidia_smi_devices(),
            "vulkan_loader": hardware::vulkan_loader_present(),
            "binaries_dir": p.binaries,
            "models_dir": p.models,
            "backends": rows.iter().map(|(b, has_bin, avail, usable)| serde_json::json!({
                "backend": b.as_str(),
                "has_binary": has_bin,
                "hardware_available": avail,
                "usable": usable,
            })).collect::<Vec<_>>(),
            "selected": effective.as_str(),
            "model_recommended": model.label(),
            "model_file_exists": p.models.join(model.file_name()).is_file(),
        });
        println!("{}", serde_json::to_string_pretty(&obj)?);
        return Ok(());
    }

    println!("=== 硬件探测 ===");
    println!("CPU 核心数   : {cores}");
    match hardware::nvidia_smi_devices() {
        Some(d) => println!("NVIDIA 设备  : {}", d.join(", ")),
        None => println!("NVIDIA 设备  : 未检测到"),
    }
    if let Some(v) = vram {
        println!("显存         : {v:.1} GB");
    }
    println!(
        "Vulkan 运行库: {}",
        if hardware::vulkan_loader_present() {
            "可用"
        } else {
            "未找到"
        }
    );
    println!();
    println!("=== 后端可用性 ===");
    println!("{:<8} {:>8} {:>10} {:>8}", "后端", "有程序", "硬件支持", "可用");
    for (b, has_bin, avail, usable) in &rows {
        println!(
            "{:<8} {:>8} {:>10} {:>8}",
            b.label(),
            if *has_bin { "是" } else { "否" },
            if *avail { "是" } else { "否" },
            if *usable { "✅" } else { "—" }
        );
    }
    println!();
    println!("二进制目录   : {}", p.binaries.display());
    println!("模型目录     : {}", p.models.display());
    println!();
    println!("=== 结论 ===");
    println!("将使用后端   : {}", effective.label());
    println!("推荐模型     : {} ({})", model.label(), model.file_name());
    let mp = p.models.join(model.file_name());
    if mp.is_file() {
        println!("模型状态     : ✅ 已就绪");
    } else {
        println!("模型状态     : ❌ 缺失 —— 需要下载到 {}", mp.display());
        println!("               下载源可用镜像:hf-mirror.com(见 README)");
    }

    if selected.is_none() {
        println!();
        println!("⚠ 未找到任何可用的 whisper-cli。");
        for (b, has_bin, avail, _) in &rows {
            if !*has_bin && *avail {
                println!("  · {}", hardware::explain_unavailable(*b, &locator));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

struct RunArgs {
    input: PathBuf,
    backend: Option<String>,
    model: Option<String>,
    language: Option<String>,
    speakers: Option<u8>,
    no_diarize: bool,
    no_summary: bool,
    terms: Vec<String>,
    chunk_secs: u64,
    print: bool,
}

fn cmd_run(p: &Paths, args: RunArgs) -> Result<()> {    if !args.input.is_file() {
        anyhow::bail!("输入文件不存在: {}", args.input.display());
    }

    let mut cfg = PipelineConfig::new(&p.data_dir)
        .with_binaries(&p.binaries)
        .with_models(&p.models);
    cfg.chunk_target_ms = args.chunk_secs.max(30) * 1000;
    cfg.hotwords = args.terms.clone();
    cfg.enable_summary = !args.no_summary;
    cfg.language = match args.language.as_deref() {
        None | Some("auto") => None,
        Some(l) => Some(l.to_string()),
    };

    if args.no_diarize {
        cfg.enable_diarize = false;
    } else if let Some(n) = args.speakers {
        cfg.diarize = DiarizeMode::Fixed(n.max(1));
    } else {
        cfg.diarize = DiarizeMode::Auto;
    }

    if let Some(b) = &args.backend {
        let backend = rs_core::types::Backend::parse(b)
            .ok_or_else(|| anyhow::anyhow!("未知后端: {b}(可选 cuda/vulkan/cpu/metal/rocm/sycl/opencl)"))?;
        cfg.backend_pref = BackendPref::Force(backend);
    }
    if let Some(m) = &args.model {
        let tier = ModelTier::parse(m)
            .ok_or_else(|| anyhow::anyhow!("未知模型: {m}"))?;
        cfg.model_override = Some(tier);
    }

    // 引擎
    let transcriber = WhisperCppSidecar::new(SidecarLocator::new(&p.binaries));

    // 总结器(可选)
    let summarizer = if cfg.enable_summary {
        match build_summarizer(&p.data_dir) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("⚠ 无法启用纪要生成:{e}");
                eprintln!("  转写仍会完成。配置好 API Key 后可重新运行以生成纪要。");
                None
            }
        }
    } else {
        None
    };

    let pipe = Pipeline::new(cfg, &transcriber, summarizer.as_ref().map(|s| s as &dyn llm::Summarizer))?;

    let input = AudioRef {
        path: args.input.clone(),
        duration_ms: None,
        content_hash: None,
        source: AudioSourceKind::File,
    };

    let sink = progress::cli_sink();
    let outcome = pipe.run(&input, &sink)?;

    // ---------- 报告 ----------
    println!();
    println!("=== 完成 ===");
    println!("会话 ID   : {}", outcome.session_id);
    println!("音频时长  : {}", format_ms(outcome.transcript.duration_ms));
    println!("段落数    : {}", outcome.transcript.segments.len());
    if let Some(d) = &outcome.transcript.diarize {
        println!("发言人数  : {}", d.num_speakers_detected);
    }
    if let Some(hw) = &outcome.hardware {
        println!("使用后端  : {}", hw.summary_line());
        println!("使用模型  : {}", hw.model_recommended.label());
    }
    if outcome.transcript_from_cache {
        println!("转写来源  : 缓存命中(未重新转写)");
    }
    if let Some(v) = &outcome.scene {
        println!(
            "场景判断  : {} (置信度 {:.0}%)",
            v.scene.label(),
            v.confidence * 100.0
        );
        if v.is_low_confidence() {
            println!("            ⚠ 判断不确定,可用 `--print` 查看后手动更换模板");
        }
    }

    let tpath = pipe.files().transcript_path(&outcome.session_id);
    println!();
    println!("转写文件  : {}", tpath.display());
    if outcome.summary.is_some() {
        println!(
            "纪要文件  : {}",
            pipe.files().summary_path(&outcome.session_id).display()
        );
    }
    println!();
    println!(
        "查看转写  : rs show {}",
        short_id(&outcome.session_id)
    );

    if args.print {
        if let Some(s) = &outcome.summary {
            println!();
            println!("{}", "=".repeat(60));
            println!("{}", s.content_md);
        }
    }

    Ok(())
}

fn build_summarizer(data_dir: &std::path::Path) -> Result<BlockingSummarizer> {
    let db = Db::open(&data_dir.join("cache.db"))?;
    let mut cfg = llm_config_from_db(&db)?;
    cfg.api_key = llm::load_api_key().ok().flatten();
    BlockingSummarizer::new(cfg, load_price_table(&db)?)
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

fn load_price_table(_db: &Db) -> Result<PriceTable> {
    Ok(PriceTable::cny())
}

// ---------------------------------------------------------------------------
// show / list / rename
// ---------------------------------------------------------------------------

fn open_files(data_dir: &std::path::Path) -> Result<FileStore> {
    let f = FileStore::new(data_dir.join("store"));
    f.ensure_dirs()?;
    Ok(f)
}

/// 允许用 ID 前缀(哈希太长,手打不现实)。
fn resolve_session(files: &FileStore, prefix: &str) -> Result<String> {
    if files.has_transcript(prefix) {
        return Ok(prefix.to_string());
    }
    let dir = files.root().join("transcript");
    let mut matches = Vec::new();
    if dir.is_dir() {
        for entry in walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
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
        _ => anyhow::bail!(
            "前缀 {prefix} 匹配到多个会话,请多给几位:\n  {}",
            matches.join("\n  ")
        ),
    }
}

fn cmd_show(
    data_dir: &std::path::Path,
    session: &str,
    view: &str,
    want_summary: bool,
) -> Result<()> {
    let files = open_files(data_dir)?;
    let id = resolve_session(&files, session)?;

    let transcript = files
        .read_transcript(&id)?
        .ok_or_else(|| anyhow::anyhow!("会话 {id} 没有转写文件"))?;
    let labels = files
        .read_labels(&id)?
        .unwrap_or_else(|| SpeakerLabels::new(id.clone()));

    let kind = ViewKind::parse(view)
        .ok_or_else(|| anyhow::anyhow!("未知视图: {view}(可选 dialogue/timeline/plain/srt)"))?;

    println!("{}", kind.render(&transcript.segments, &labels));

    if want_summary {
        if let Some(md) = files.read_summary_markdown(&id)? {
            println!();
            println!("{}", "=".repeat(60));
            println!("{md}");
        } else {
            eprintln!("\n(该会话没有纪要文件)");
        }
    }
    Ok(())
}

fn cmd_list(data_dir: &std::path::Path, limit: usize) -> Result<()> {
    let db = Db::open(&data_dir.join("cache.db"))?;
    let rows = db.list_sessions(limit)?;
    if rows.is_empty() {
        println!("(还没有处理过的录音)");
        println!("\n试试: rs run <音频文件>");
        return Ok(());
    }

    println!(
        "{:<10} {:<22} {:>8} {:<6} {}",
        "会话 ID", "标题", "时长", "场景", "状态"
    );
    println!("{}", "-".repeat(76));
    for r in rows {
        println!(
            "{:<10} {:<22} {:>8} {:<6} {}",
            short_id(&r.id),
            truncate(&r.title.unwrap_or_else(|| "(未命名)".into()), 20),
            format_ms(r.duration_ms),
            r.scene.map(|s| s.label()).unwrap_or("—"),
            r.status
        );
    }
    Ok(())
}

fn cmd_rename(
    data_dir: &std::path::Path,
    session: &str,
    speaker_id: u32,
    name: &str,
) -> Result<()> {
    let files = open_files(data_dir)?;
    let id = resolve_session(&files, session)?;

    let transcript = files
        .read_transcript(&id)?
        .ok_or_else(|| anyhow::anyhow!("会话 {id} 没有转写文件"))?;

    // ★ 没有说话人信息时要明确说明原因,而不是静默报告"名字未变化" ——
    //   后者会让用户以为程序坏了。
    if !transcript.has_speakers() {
        anyhow::bail!(
            "该会话的转写里没有发言人信息,无法改名。\n\n\
             可能原因:\n\
             · 处理时跳过了说话人区分(用了 --no-diarize)\n\
             · 声纹模型未就位,该阶段被自动跳过\n\n\
             解决办法:把 segmentation-3.0.onnx 与 3dspeaker.onnx 放到模型目录后,\n\
             重新运行 `rs run <原音频>` 即可补做说话人区分(转写本身会命中缓存,很快)。"
        );
    }

    let ids: Vec<u32> = transcript
        .segments
        .iter()
        .filter_map(|s| s.speaker_id)
        .collect();

    let mut labels = files
        .read_labels(&id)?
        .unwrap_or_else(|| SpeakerLabels::new(id.clone()));
    labels.ensure(ids);

    // 说话人序号必须真实存在
    if !labels.labels.contains_key(&speaker_id) {
        let mut available: Vec<u32> = labels.labels.keys().copied().collect();
        available.sort_unstable();
        anyhow::bail!(
            "没有编号为 {speaker_id} 的发言人。\n现有:{}",
            available
                .iter()
                .map(|i| format!("[{i}] {}", labels.display_name(*i)))
                .collect::<Vec<_>>()
                .join("  ")
        );
    }

    let old = labels.display_name(speaker_id);
    if old == name {
        println!("名字未变化:{old}");
        return Ok(());
    }
    labels.rename(speaker_id, name);
    files.write_labels(&labels)?;

    println!("已把「{old}」改名为「{name}」");
    println!();

    // ★ 改名会改标签版本 → 已生成的纪要通过该字段判断是否过时
    if let Some(sum) = files.read_summary_meta(&id)? {
        if sum.is_stale(&labels) {
            println!("⚠ 提示:这个名字已用于生成过纪要,当前纪要里仍是旧名字。");
            println!("   重新生成纪要即可生效(转写缓存在,只需几秒):");
            println!("     rs run <原音频>");
        }
    }

    println!("现有发言人:");
    for (sid, l) in &labels.labels {
        println!("  [{sid}] {}", l.display_name);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// profiles
// ---------------------------------------------------------------------------

fn cmd_profiles(data_dir: &std::path::Path, action: Option<ProfileAction>) -> Result<()> {
    let files = open_files(data_dir)?;
    let db = Db::open(&data_dir.join("cache.db"))?;
    let vs = rs_core::voiceprint::VoiceprintStore::new(&db, &files);

    match action {
        None | Some(ProfileAction::List) => {
            let list = vs.list_profiles()?;
            if list.is_empty() {
                println!("(还没有声纹档案)");
                println!();
                println!("档案会在你给发言人改名字时自动建立 —— 改名即登记。");
                println!("这样下次录到同一个人时会自动识别并标注。");
                return Ok(());
            }
            println!("{:<16} {:<14} {:>8} {:<12}", "档案 ID", "名字", "样本数", "模型");
            println!("{}", "-".repeat(54));
            for p in list {
                let (n, ms) = vs.sample_stats(&p.profile_id)?;
                let dur = if ms > 0 {
                    format!("{} 分钟", ms / 60_000)
                } else {
                    "—".into()
                };
                println!(
                    "{:<16} {:<14} {:>8} {:<12} {}",
                    p.profile_id,
                    truncate(&p.display_name, 12),
                    n,
                    p.embedding_model,
                    dur
                );
            }
            println!();
            println!("注:声纹向量只保存在本机,不会随同步上传。");
        }
        Some(ProfileAction::Delete { profile_id }) => {
            vs.delete_profile(&profile_id)?;
            println!("已删除档案 {profile_id}(含全部登记样本)");
        }
        Some(ProfileAction::Rename { profile_id, name }) => {
            vs.rename_profile(&profile_id, &name)?;
            println!("已改名为「{name}」");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// projects
// ---------------------------------------------------------------------------

fn cmd_projects(data_dir: &std::path::Path, action: Option<ProjectAction>) -> Result<()> {
    use rs_core::project::{ProjectStore, SummaryKind};

    let files = open_files(data_dir)?;
    let store = ProjectStore::new(files.root());

    match action.unwrap_or(ProjectAction::List) {
        ProjectAction::List => {
            let list = store.list()?;
            if list.is_empty() {
                println!("(还没有工程)");
                println!();
                println!("工程会在处理录音时自动建立:");
                println!("  rs run <音频文件>");
                return Ok(());
            }

            println!(
                "{:<10} {:<26} {:>8} {:<6} {}",
                "工程 ID", "标题", "时长", "场景", "产物"
            );
            println!("{}", "-".repeat(88));
            for p in &list {
                let missing = p.meta.artifacts.missing();
                let status = if missing.is_empty() {
                    "完整".to_string()
                } else {
                    format!("缺:{}", missing.join("/"))
                };
                println!(
                    "{:<10} {:<26} {:>8} {:<6} {}",
                    truncate(&p.meta.id, 8),
                    truncate(&p.meta.title, 24),
                    format_ms(p.meta.duration_ms),
                    p.meta.scene.map(|s| s.label()).unwrap_or("—"),
                    status
                );
            }
            println!();
            println!("目录:{}", store.projects_dir().display());
            println!("用 `rs projects show <ID>` 看详情");
        }

        ProjectAction::Show { id } => {
            let p = store
                .find(&id)?
                .ok_or_else(|| anyhow::anyhow!("找不到工程: {id}"))?;

            println!("标题     : {}", p.meta.title);
            println!("工程 ID  : {}", p.meta.id);
            println!("目录     : {}", p.dir.display());
            println!("时长     : {}", format_ms(p.meta.duration_ms));
            println!(
                "创建时间 : {}",
                format_time(p.meta.created_at)
            );
            if let Some(s) = p.meta.scene {
                let conf = p
                    .meta
                    .scene_confidence
                    .map(|c| format!(" ({:.0}%)", c * 100.0))
                    .unwrap_or_default();
                println!("场景     : {}{conf}", s.label());
            }
            if let Some(m) = &p.meta.asr_model {
                println!(
                    "转写     : {m} / {}",
                    p.meta.asr_backend.as_deref().unwrap_or("?")
                );
            }
            if p.meta.has_speakers {
                println!("发言人   : {} 位", p.meta.speaker_count);
            }

            println!();
            println!("产物:");
            let a = &p.meta.artifacts;
            let mark = |b: bool| if b { "✅" } else { "—" };
            println!("  {} audio/            录音(工程自包含)", mark(a.audio));
            println!("  {} transcript.md     带时间戳的转写", mark(a.transcript));
            println!("  {} transcript.json   结构化段落", mark(a.transcript_json));
            println!("  {} transcript.srt    字幕", mark(a.transcript_srt));
            println!("  {} summary-detailed  详细总结", mark(a.summary_detailed));
            println!("  {} summary-brief     简略总结", mark(a.summary_brief));
            println!("  {} mindmap.mmd       思维导图", mark(a.mindmap));

            let missing = a.missing();
            if !missing.is_empty() {
                println!();
                println!("⚠ 缺少:{}", missing.join("、"));
                println!("  重新处理同一音频即可补齐(转写会命中缓存)");
            }
        }

        ProjectAction::Cat { id, what } => {
            let p = store
                .find(&id)?
                .ok_or_else(|| anyhow::anyhow!("找不到工程: {id}"))?;

            let content = match what.to_ascii_lowercase().as_str() {
                "brief" | "简略" => p.read_summary(SummaryKind::Brief),
                "detailed" | "detail" | "详细" => p.read_summary(SummaryKind::Detailed),
                "transcript" | "转写" => p.read_transcript_md(),
                "srt" | "字幕" => p.read_srt(),
                "mindmap" | "导图" => p.read_mindmap(),
                "outline" | "大纲" => {
                    std::fs::read_to_string(p.dir.join("mindmap-outline.md")).ok()
                }
                other => anyhow::bail!(
                    "未知产物: {other}\n可选:brief | detailed | transcript | srt | mindmap | outline"
                ),
            };

            match content {
                Some(c) => println!("{c}"),
                None => {
                    anyhow::bail!(
                        "工程「{}」里没有 {what},可能还没生成。\n用 `rs projects show {}` 看产物清单。",
                        p.meta.title,
                        id
                    )
                }
            }
        }

        ProjectAction::Export { id, dest } => {
            let p = store
                .find(&id)?
                .ok_or_else(|| anyhow::anyhow!("找不到工程: {id}"))?;

            let target = dest.join(&p.meta.slug);
            if target.exists() {
                anyhow::bail!("目标已存在:{}", target.display());
            }
            copy_dir_recursive(&p.dir, &target)?;

            let (files, bytes) = dir_stats(&target);
            println!("✅ 已导出到 {}", target.display());
            println!("   {files} 个文件,共 {}", human_size(bytes));
            println!();
            println!("这个目录是自包含的 —— 可以直接打包发给别人。");
        }

        ProjectAction::Rename { id, title } => {
            let p = store
                .find(&id)?
                .ok_or_else(|| anyhow::anyhow!("找不到工程: {id}"))?;
            let old_title = p.meta.title.clone();
            let old_dir = p.dir.clone();

            let out = store.rename(&id, &title)?;

            println!("✅ 已重命名");
            println!("   标题: {old_title} → {}", store.find(&id)?.unwrap().meta.title);
            if out.slug_changed {
                println!("   目录: {}", old_dir.display());
                println!("      → {}", out.dir.display());
                println!();
                // ★ 必须提醒:目录名变了 = 云端路径全变
                println!("⚠ 目录名变了,云端的路径也跟着变。");
                println!("   如果这个工程已经同步过,云端旧路径下的文件会变成孤儿。");
                println!("   下次 `rs sync run` 会把新路径传上去;旧路径需要手动清理");
                println!("   (或等同步的删除策略处理 —— 但那只对在范围内的文件生效)。");
            } else {
                println!("   (目录名未变,只有标题更新了)");
            }
        }
    }
    Ok(())
}

/// 递归拷目录。
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let from = e.path();
        let to = dst.join(e.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn dir_stats(dir: &std::path::Path) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                let (f, b) = dir_stats(&p);
                files += f;
                bytes += b;
            } else if let Ok(m) = e.metadata() {
                files += 1;
                bytes += m.len();
            }
        }
    }
    (files, bytes)
}

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// 列表里的字节数:0 显示为 `—`,避免一堆 "0 B" 干扰阅读。
fn bytes_text(bytes: u64) -> String {
    if bytes == 0 {
        "—".to_string()
    } else {
        human_size(bytes)
    }
}

/// 毫秒时间戳 → 本地可读时间。
fn format_time(ms: i64) -> String {
    // 不引 chrono:只需要一个可读串
    let secs = (ms / 1000).max(0) as u64;
    let days = secs / 86_400;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        tod / 3600,
        (tod % 3600) / 60
    )
}

/// 天数 → (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// sync
// ---------------------------------------------------------------------------

/// WebDAV 凭据同样走系统凭据管理器,不进配置文件。
const KEYRING_WEBDAV: &str = "webdav-password";

fn webdav_config_from_db(db: &Db) -> Result<rs_core::sync::WebDavConfig> {
    let mut cfg = rs_core::sync::WebDavConfig::default();
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
            keyring::Entry::new(crate::llm::KEYRING_SERVICE, KEYRING_WEBDAV)
                .ok()
                .and_then(|e| e.get_password().ok())
        })
        .unwrap_or_default();
    Ok(cfg)
}

fn cmd_sync(data_dir: &std::path::Path, action: SyncAction) -> Result<()> {
    let db = Db::open(&data_dir.join("cache.db"))?;
    let files = open_files(data_dir)?;

    match action {
        SyncAction::Login { username, url, dir } => {
            // ★ 允许把一整条地址粘进 --url。
            //
            // 用户脑子里想的是 `https://cloud.example.com/seafdav/recording_summary`
            // 这一条,而不是"服务地址"+"远端目录"两个框。
            // 不拆的话会拼出 .../seafdav/recording_summary/recording_summary。
            let (url, dir) = match rs_core::sync::split_webdav_url(&url) {
                Some((base, extracted)) => {
                    // 用户另外显式给了 --dir 时以他为准,但要提醒
                    if dir != "recording-summary" && dir != extracted {
                        println!(
                            "⚠ --url 里含子目录「{extracted}」,但 --dir 给的是「{dir}」,以 --dir 为准"
                        );
                        (base, dir)
                    } else {
                        println!("ℹ 已从地址里识别出远端目录:{extracted}");
                        (base, extracted)
                    }
                }
                None => (url, dir),
            };

            println!("WebDAV 密码(输入不会显示):");
            let password = rpassword_read()?;
            if password.trim().is_empty() {
                anyhow::bail!("密码为空,已取消");
            }
            db.set_setting("webdav.username", &username)?;
            db.set_setting("webdav.url", &url)?;
            db.set_setting("webdav.dir", &dir)?;
            let entry = keyring::Entry::new(crate::llm::KEYRING_SERVICE, KEYRING_WEBDAV)
                .map_err(|e| anyhow::anyhow!("无法访问系统凭据管理器: {e}"))?;
            entry
                .set_password(&password)
                .map_err(|e| anyhow::anyhow!("写入凭据管理器失败: {e}"))?;

            let cfg = rs_core::sync::WebDavConfig {
                base_url: url.clone(),
                username: username.clone(),
                password: String::new(),
                remote_dir: dir.clone(),
                timeout_secs: 60,
            };

            println!("✅ 已保存(密码在系统凭据管理器,不在配置文件里)");
            println!("   服务地址 : {url}");
            println!("   远端目录 : {dir}");
            println!("   用户     : {username}");
            println!();
            println!("文件会上传到:");
            println!("   {}", cfg.root());
            println!();
            println!("下一步:rs sync test");
        }

        SyncAction::Show { env } => {
            let cfg = webdav_config_from_db(&db)?;
            if env {
                // 给诊断程序用。故意不做任何美化 —— 便于 `| Invoke-Expression`
                // 或 `for /f` 直接消费。
                println!("DIAG_URL={}", cfg.base_url);
                println!("DIAG_USER={}", cfg.username);
                println!("DIAG_PASS={}", cfg.password);
                println!("DIAG_DIR={}", cfg.remote_dir);
                return Ok(());
            }
            println!("服务地址   : {}", cfg.base_url);
            println!("远端目录   : {}", cfg.remote_dir);
            println!("上传目标   : {}", cfg.root());
            println!("用户名     : {}", if cfg.username.is_empty() { "(未设置)" } else { &cfg.username });
            println!(
                "密码       : {}",
                if cfg.password.is_empty() {
                    "(未设置)"
                } else {
                    "(已保存到凭据管理器)"
                }
            );
            println!();
            println!("本地目录   : {}", files.root().display());
            println!();
            println!("注意:音频是否走 WebDAV 取决于 `--audio`(默认参与)。见 `rs sync plan`。");
            println!("     要看明文密码(诊断用)加 `--env`。");
        }

        SyncAction::Test => {
            let cfg = webdav_config_from_db(&db)?;
            let syncer = rs_core::sync::Syncer::new(cfg, &files)?;
            match syncer.test_connection() {
                Ok(msg) => println!("✅ {msg}"),
                Err(e) => {
                    println!("❌ 连接失败:{e}");
                    return Err(e);
                }
            }
        }

        SyncAction::Plan {
            include,
            exclude,
            projects,
            audio,
            deletion,
            direction,
            offline,
        } => {
            let sel = build_selection(&include, &exclude, &projects, &audio, &deletion, &direction)?;
            // 离线模式不读凭据 —— 没配账号也能看删除判定
            let syncer = if offline {
                rs_core::sync::Syncer::new_offline(&files)?.with_selection(sel)
            } else {
                let cfg = webdav_config_from_db(&db)?;
                rs_core::sync::Syncer::new(cfg, &files)?.with_selection(sel)
            };
            println!("同步范围:{}", syncer.selection().describe());
            println!("本机标识:{}", syncer.device_id());
            if offline {
                println!("模式      :离线(假设远端为空,不发网络请求)");
            }
            println!();

            let manifest = rs_core::sync::manifest::Manifest::load_or_new(&files.manifest_path())?;
            let plan = if offline {
                syncer.plan_offline(&manifest)?
            } else {
                syncer.plan(&manifest)?
            };

            if plan.is_empty() {
                println!("(没有符合范围的文件)");
            } else {
                let up = plan
                    .iter()
                    .filter(|i| i.action == rs_core::sync::Action::Upload)
                    .count();
                let down = plan
                    .iter()
                    .filter(|i| i.action == rs_core::sync::Action::Download)
                    .count();
                let skip = plan
                    .iter()
                    .filter(|i| i.action == rs_core::sync::Action::Skip)
                    .count();
                let del = plan
                    .iter()
                    .filter(|i| i.action == rs_core::sync::Action::DeleteRemote)
                    .count();
                println!("待上传 {up} · 待下载 {down} · 待删除 {del} · 跳过 {skip}");
                println!();
                println!("{:<56} {:<8} {:>10}", "文件", "动作", "大小");
                println!("{}", "-".repeat(80));
                for item in &plan {
                    let act = match item.action {
                        rs_core::sync::Action::Upload => "上传",
                        rs_core::sync::Action::Download => "下载",
                        rs_core::sync::Action::Skip => "跳过",
                        rs_core::sync::Action::DeleteRemote => "★删除",
                    };
                    // 跳过的太多,只显示需要动的
                    if item.action == rs_core::sync::Action::Skip {
                        continue;
                    }
                    println!(
                        "{:<56} {:<8} {:>10}",
                        truncate(&item.rel_path, 54),
                        act,
                        bytes_text(item.local_size)
                    );
                }
                if del > 0 {
                    println!();
                    println!("⚠ 有 {del} 个文件将被从云端删除(本地已不存在)。");
                    println!("  这是 `--deletion` 策略允许的结果。要保留云端副本请用 `--deletion keep`。");
                }
            }
        }

        SyncAction::Run {
            include,
            exclude,
            projects,
            audio,
            deletion,
            direction,
        } => {
            let cfg = webdav_config_from_db(&db)?;
            let sel = build_selection(&include, &exclude, &projects, &audio, &deletion, &direction)?;
            let syncer = rs_core::sync::Syncer::new(cfg, &files)?.with_selection(sel);
            println!("同步范围:{}", syncer.selection().describe());
            println!("开始同步...");
            let report = syncer.run(&|path, i, total| {
                eprint!("\r[{i}/{total}] {}", truncate(path, 50));
            })?;
            eprintln!();
            println!("✅ {}", report.summary());
            if report.deleted > 0 {
                println!();
                println!("已按策略从云端删除 {} 个文件(本地已不存在)。", report.deleted);
            }
            if !report.failed.is_empty() {
                println!();
                println!("失败明细:");
                for (p, e) in &report.failed {
                    println!("  {p}");
                    println!("    {e}");
                }
            }
        }

        SyncAction::Rebuild => {
            let cfg = webdav_config_from_db(&db)?;
            let syncer = rs_core::sync::Syncer::new(cfg, &files)?;
            println!("从远端扫描重建 manifest...");
            let m = syncer.rebuild_manifest()?;
            m.save(&files.manifest_path())?;
            println!("✅ 已重建,共 {} 条记录", m.len());
            println!("   保存到 {}", files.manifest_path().display());
        }
    }
    Ok(())
}

/// 从 CLI 参数构造同步选择。
fn build_selection(
    include: &[String],
    exclude: &[String],
    projects: &[String],
    audio: &Option<String>,
    deletion: &Option<String>,
    direction: &Option<String>,
) -> Result<rs_core::sync::selection::SyncSelection> {
    use rs_core::sync::selection::{AudioPolicy, DeletionPolicy, Direction};

    let audio_policy = match audio {
        Some(s) => Some(AudioPolicy::parse(s).ok_or_else(|| {
            anyhow::anyhow!("--audio 取值无效:{s}\n可选:skip | upload | two-way")
        })?),
        None => None,
    };
    let del_policy = match deletion {
        Some(s) => Some(DeletionPolicy::parse(s).ok_or_else(|| {
            anyhow::anyhow!("--deletion 取值无效:{s}\n可选:keep | text | mirror")
        })?),
        None => None,
    };
    let dir = match direction {
        Some(s) => Some(Direction::parse(s).ok_or_else(|| {
            anyhow::anyhow!(
                "--direction 取值无效:{s}\n可选:both | upload(只上传) | download(只下载)"
            )
        })?),
        None => None,
    };

    let mut sel = rs_core::sync::selection::SyncSelection::from_args(
        include,
        exclude,
        projects,
        audio_policy,
        del_policy,
    );
    if let Some(d) = dir {
        sel.direction = d;
    }

    // 只下载时提醒:删除不会传播
    if sel.direction == Direction::DownloadOnly && del_policy.is_some() {
        eprintln!("ℹ --direction download:只下载模式不会删除云端,删除策略被忽略。");
    }

    // 提醒用户:mirror 会连音频一起删,风险高
    if del_policy == Some(DeletionPolicy::Mirror) {
        eprintln!("⚠ --deletion mirror:本地删除会同步到云端,**包括录音**。");
        eprintln!("  用 `rs sync plan` 先看一眼计划再执行。");
        eprintln!();
    }
    Ok(sel)
}

/// 读一行不显示的输入(密码)。
///
/// 不引入额外 crate:`rpassword` 会再拉几个依赖,而这里只需要简单实现。
fn rpassword_read() -> Result<String> {
    // Windows 下没有便携的 termios;退化为普通读取并提示
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim_end_matches(['\r', '\n']).to_string())
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

fn cmd_config(data_dir: &std::path::Path, action: ConfigAction) -> Result<()> {
    let db = Db::open(&data_dir.join("cache.db"))?;

    match action {
        ConfigAction::Show => {
            let cfg = llm_config_from_db(&db)?;
            println!("数据目录   : {}", data_dir.display());
            println!("供应商     : {}", cfg.provider);
            println!("Base URL   : {}", cfg.base_url);
            println!("模型       : {}   ({})", cfg.model, cfg.model_hint());
            match llm::load_api_key().ok().flatten() {
                Some(k) => println!("API Key    : {}", llm::keyring::mask(&k)),
                None => println!("API Key    : (未配置)"),
            }
            if llm::keyring::api_key_from_env().is_some() {
                println!("             (来自环境变量 {})", llm::keyring::ENV_VAR);
            }
            println!();
            println!("内置预设(可用 --help 查看全部):");
            for (name, url, models) in LlmConfig::presets() {
                println!("  {name:<10} {url}");
                println!("             {}", models.join(", "));
            }
        }
        ConfigAction::SetKey { key } => {
            llm::store_api_key(&key)?;
            println!("✅ 已保存到系统凭据管理器(未写入任何配置文件)");
            println!("   验证:rs config test");
        }
        ConfigAction::ClearKey => {
            let removed = llm::delete_api_key()?;
            if removed {
                println!("✅ 已删除");
            } else {
                println!("(本来就没有保存过)");
            }
        }
        ConfigAction::Test => {
            let mut cfg = llm_config_from_db(&db)?;
            cfg.api_key = llm::load_api_key()?;
            let sum = BlockingSummarizer::new(cfg, PriceTable::cny())?;
            match sum.test_connection_blocking() {
                Ok(reply) => {
                    println!("✅ 连接正常,模型回复:{reply}");
                }
                Err(e) => {
                    println!("❌ 连接失败:{e}");
                    return Err(e);
                }
            }
        }
        ConfigAction::SetBaseUrl { url } => {
            db.set_setting("llm.base_url", &url)?;
            println!("✅ Base URL 已设为 {url}");
            println!("   提示:换供应商后记得同步改模型名。");
        }
        ConfigAction::SetModel { model } => {
            db.set_setting("llm.model", &model)?;
            println!("✅ 模型已设为 {model}");
        }

        ConfigAction::DataDir { action } => {
            // 这个 match 求值为 () —— cmd_config 的其它分支也返回 ()
            match action.unwrap_or(DataDirAction::Show) {
                DataDirAction::Show => data_dir_show(data_dir),
                DataDirAction::Reset => data_dir_reset()?,
                DataDirAction::Move { path } => {
                    // ★ 先关掉数据库并做 WAL checkpoint,否则 -wal 里
                    //   未合并的事务不会被完整拷走
                    drop(db);
                    if let Err(e) = rs_core::checkpoint_sqlite(data_dir) {
                        println!("⚠ 数据库 checkpoint 失败({e}),继续尝试迁移");
                    }
                    data_dir_move(data_dir, &path)?;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 数据目录
// ---------------------------------------------------------------------------

/// 数据目录的位置由**指针文件**决定,而指针文件不在数据目录里 ——
/// 否则就是先有鸡还是先有蛋。它固定在
/// `%APPDATA%\recording-summary\location.txt`。
fn data_dir_show(current: &std::path::Path) {
    let custom = rs_core::data_dir_is_custom();
    println!("当前数据目录 : {}", current.display());
    println!(
        "来源         : {}",
        if custom {
            "自定义(指针文件)"
        } else {
            "系统默认"
        }
    );
    println!("指针文件     : {}", rs_core::location_file().display());
    println!("系统默认位置 : {}", rs_core::system_data_dir().display());
    println!();
    if current.is_dir() {
        let mut files = 0usize;
        let mut bytes = 0u64;
        for e in walkdir::WalkDir::new(current)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            files += 1;
            bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
        println!("占用         : {files} 个文件,共 {}", human_size(bytes));
    } else {
        println!("(目录尚不存在)");
    }
    println!();
    println!("改到别处:`rs config data-dir move D:\\RecordingSummary`");
}

fn data_dir_reset() -> Result<()> {
    let sys = rs_core::system_data_dir();
    rs_core::reset_data_dir()?;
    println!("✅ 已删除位置指针,回到系统默认");
    println!("   现在数据目录是:{}", sys.display());
    println!();
    if !sys.is_dir() {
        println!("   (该目录还不存在,下次运行时会自动创建)");
    }
    println!("注意:**没有**帮你搬数据。要搬用 `rs config data-dir move`。");
    Ok(())
}

fn data_dir_move(src: &std::path::Path, path: &std::path::Path) -> Result<()> {
    println!("把数据从");
    println!("  {}", src.display());
    println!("迁到");
    println!("  {}", path.display());
    println!();

    // 目标已有数据时提醒 —— 合并两边几乎肯定不是用户想要的
    if path.join("cache.db").exists() || path.join("store").is_dir() {
        println!("⚠ 目标目录里已经有数据(cache.db 或 store/)。");
        println!("  继续会把两边**合并**,同名文件以源为准。");
        println!();
        print!("确定继续?输入 yes:");
        use std::io::BufRead;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        if line.trim() != "yes" {
            println!("已取消。");
            return Ok(());
        }
    }

    let outcome = rs_core::migrate_data_dir(src, path)?;
    println!(
        "✅ 已迁移 {} 个文件,共 {}",
        outcome.files,
        human_size(outcome.bytes)
    );
    println!("   新位置: {}", path.display());
    match &outcome.cleanup_warning {
        Some(w) => {
            println!();
            println!("⚠ {w}");
        }
        None => println!("   旧目录已删除"),
    }
    println!();
    println!("之后不用再加 --data-dir,程序会自动用新位置。");
    println!("想改回系统默认:`rs config data-dir reset`");
    Ok(())
}

// ---------------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------------

fn cmd_stats(data_dir: &std::path::Path) -> Result<()> {
    let db = Db::open(&data_dir.join("cache.db"))?;
    let s = cache::stats(&db)?;
    let files = open_files(data_dir)?;

    println!("=== 缓存统计 ===");
    println!("数据目录     : {}", data_dir.display());
    println!("会话数       : {}", s.sessions);
    println!("转写缓存块   : {}", s.transcript_chunks);
    println!("说话人区分   : {}", s.diarize_results);
    println!("登记样本     : {}", s.enrollment_samples);

    let sync_files = files.list_sync_files()?;
    let total: u64 = sync_files
        .iter()
        .filter_map(|f| std::fs::metadata(f).ok())
        .map(|m| m.len())
        .sum();
    println!();
    println!("待同步文本   : {} 个文件,共 {:.1} MB", sync_files.len(), total as f64 / 1_048_576.0);
    println!("               (转写/纪要/名字映射;音频由 Seafile 客户端负责)");
    Ok(())
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn truncate(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}…")
    }
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
