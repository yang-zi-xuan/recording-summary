//! 工程制存储。见技术方案 §17。
//!
//! # 为什么要有"工程"
//!
//! 原来的存储是**按内容哈希打散**的:
//!
//! ```text
//! store/transcript/ab/ab3f9c...json
//! store/summary/ab/ab3f9c...md
//! store/speaker_labels/ab/ab3f9c...json
//! ```
//!
//! 这样做的理由很实在:两台设备录到同一节课只存一份,而且不需要列目录
//! (路径就是哈希,直接 PROPFIND 那一个路径)。**但它不是给人看的。**
//!
//! 工程制是**面向用户的视图**:
//!
//! ```text
//! projects/2026-09-11_高等数学第12讲/
//! ├─ project.json          元数据
//! ├─ audio/recording.m4a   录音副本(工程自包含)
//! ├─ transcript.md         带时间戳的转写
//! ├─ transcript.json       结构化段落
//! ├─ transcript.srt        字幕
//! ├─ summary-detailed.md   详细总结
//! ├─ summary-brief.md      简略总结
//! └─ mindmap.mmd           思维导图(Mermaid 源码)
//! ```
//!
//! **整个目录可以直接打包发给同学,也能被 WebDAV 原样同步。**
//!
//! # 与哈希存储的关系
//!
//! 两者**并存**,不是替代:
//! - 工程目录:面向用户,可打包、可同步、可阅读
//! - 哈希存储:内部去重与缓存键,保证同一音频不重复转写
//!
//! 工程的 `id` 就是音频内容哈希 —— 所以"同一节课录两次"会落到同一个工程。

use crate::types::{Scene, SpeakerLabels, Summary, Transcript};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 工程目录的根名字。
pub const PROJECTS_DIR: &str = "projects";

/// 工程元数据,存 `project.json`。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectMeta {
    pub version: u32,
    /// 工程 ID = 音频内容哈希。同一音频重复处理会落到同一工程。
    pub id: String,
    /// 面向用户的标题
    pub title: String,
    /// 目录名(可能因为重名而带序号)
    pub slug: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene: Option<Scene>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scene_confidence: Option<f32>,
    /// 原始文件路径(仅作记录,工程本身不依赖它)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    /// 转写用的模型与后端
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asr_backend: Option<String>,
    /// 是否含说话人信息
    #[serde(default)]
    pub has_speakers: bool,
    #[serde(default)]
    pub speaker_count: u32,
    /// 各产物是否已生成 —— 用于 UI 显示"缺什么"
    #[serde(default)]
    pub artifacts: Artifacts,
}

/// 工程里有哪些产物。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Artifacts {
    pub audio: bool,
    pub transcript: bool,
    pub transcript_json: bool,
    pub transcript_srt: bool,
    pub summary_detailed: bool,
    pub summary_brief: bool,
    pub mindmap: bool,
}

impl Artifacts {
    /// 探测目录里实际存在哪些文件。
    pub fn detect(dir: &Path) -> Self {
        let has = |p: PathBuf| p.is_file();
        Self {
            audio: audio_path(dir).is_some(),
            transcript: has(dir.join(TRANSCRIPT_MD)),
            transcript_json: has(dir.join(TRANSCRIPT_JSON)),
            transcript_srt: has(dir.join(TRANSCRIPT_SRT)),
            summary_detailed: has(dir.join(SUMMARY_DETAILED)),
            summary_brief: has(dir.join(SUMMARY_BRIEF)),
            mindmap: has(dir.join(MINDMAP)),
        }
    }

    /// 缺了哪些产物(给用户的提示)。
    pub fn missing(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if !self.audio {
            v.push("录音");
        }
        if !self.transcript {
            v.push("转写");
        }
        if !self.summary_detailed {
            v.push("详细总结");
        }
        if !self.summary_brief {
            v.push("简略总结");
        }
        if !self.mindmap {
            v.push("思维导图");
        }
        v
    }
}

// 固定文件名
pub const TRANSCRIPT_MD: &str = "transcript.md";
pub const TRANSCRIPT_JSON: &str = "transcript.json";
pub const TRANSCRIPT_SRT: &str = "transcript.srt";
pub const SUMMARY_DETAILED: &str = "summary-detailed.md";
pub const SUMMARY_BRIEF: &str = "summary-brief.md";
pub const MINDMAP: &str = "mindmap.mmd";
pub const META_JSON: &str = "project.json";
pub const AUDIO_DIR: &str = "audio";

/// 找工程里的录音文件(扩展名不定)。
pub fn audio_path(dir: &Path) -> Option<PathBuf> {
    let a = dir.join(AUDIO_DIR);
    if !a.is_dir() {
        return None;
    }
    for e in std::fs::read_dir(&a).ok()?.filter_map(|e| e.ok()) {
        let p = e.path();
        if p.is_file() {
            // 跳过校验文件
            if p.file_name().map(|n| n == "audio.sha256").unwrap_or(false) {
                continue;
            }
            return Some(p);
        }
    }
    None
}

/// 把标题变成安全的目录名。
///
/// 规则:去掉路径非法字符、压掉连续空白、限制长度;空标题给个兜底名。
pub fn slugify(title: &str, max_chars: usize) -> String {
    const ILLEGAL: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

    let cleaned: String = title
        .chars()
        .map(|c| {
            if ILLEGAL.contains(&c) || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();

    // 压掉连续空白并去首尾
    let mut out = String::new();
    let mut prev_space = false;
    for c in cleaned.chars() {
        if c.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    let out = out.trim().trim_end_matches('.').to_string();

    // 截断(按字符,不按字节,避免切断中文)
    let truncated: String = out.chars().take(max_chars).collect();
    let truncated = truncated.trim().to_string();

    if truncated.is_empty() {
        "未命名录音".to_string()
    } else {
        truncated
    }
}

/// 工程句柄。
#[derive(Clone, Debug)]
pub struct Project {
    pub dir: PathBuf,
    pub meta: ProjectMeta,
}

impl Project {
    pub fn audio_path(&self) -> Option<PathBuf> {
        audio_path(&self.dir)
    }

    pub fn read_summary(&self, kind: SummaryKind) -> Option<String> {
        std::fs::read_to_string(self.dir.join(kind.file_name())).ok()
    }

    pub fn read_transcript_md(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join(TRANSCRIPT_MD)).ok()
    }

    pub fn read_srt(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join(TRANSCRIPT_SRT)).ok()
    }

    pub fn read_mindmap(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join(MINDMAP)).ok()
    }

    /// 保存元数据(原子写)。
    pub fn save_meta(&self) -> Result<()> {
        crate::store::files::write_json(&self.dir.join(META_JSON), &self.meta)
    }

    /// 重新探测产物并刷新 meta。
    pub fn refresh_artifacts(&mut self) -> Result<()> {
        self.meta.artifacts = Artifacts::detect(&self.dir);
        self.meta.updated_at = crate::store::db::now_ms();
        self.save_meta()
    }
}

/// 两种总结。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SummaryKind {
    /// 详细:完整结构(知识点/例题/待办/复习提纲)
    Detailed,
    /// 简略:一句话摘要 + 要点 + 思维导图
    Brief,
}

impl SummaryKind {
    pub fn file_name(&self) -> &'static str {
        match self {
            SummaryKind::Detailed => SUMMARY_DETAILED,
            SummaryKind::Brief => SUMMARY_BRIEF,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            SummaryKind::Detailed => "详细总结",
            SummaryKind::Brief => "简略总结",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "detailed" | "detail" | "详细" => Some(SummaryKind::Detailed),
            "brief" | "short" | "简略" | "简略总结" => Some(SummaryKind::Brief),
            _ => None,
        }
    }
}

/// 工程仓库:负责创建、查找、列出。
pub struct ProjectStore {
    root: PathBuf,
}

impl ProjectStore {
    pub fn new(store_root: impl Into<PathBuf>) -> Self {
        Self {
            root: store_root.into(),
        }
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.root.join(PROJECTS_DIR)
    }

    /// 按工程 ID 找目录(遍历 meta,因为目录名可能带序号)。
    pub fn find(&self, id_prefix: &str) -> Result<Option<Project>> {
        let base = self.projects_dir();
        if !base.is_dir() {
            return Ok(None);
        }
        let mut matches = Vec::new();
        for e in std::fs::read_dir(&base)?.filter_map(|e| e.ok()) {
            if !e.path().is_dir() {
                continue;
            }
            if let Ok(Some(p)) = load_project(&e.path()) {
                if p.meta.id == id_prefix || p.meta.id.starts_with(id_prefix) {
                    matches.push(p);
                }
            }
        }
        match matches.len() {
            0 => Ok(None),
            1 => Ok(Some(matches.remove(0))),
            _ => Err(anyhow!(
                "工程 ID 前缀 {id_prefix} 匹配到多个工程,请多给几位"
            )),
        }
    }

    /// 列出全部工程(按更新时间倒序)。
    pub fn list(&self) -> Result<Vec<Project>> {
        let base = self.projects_dir();
        if !base.is_dir() {
            return Ok(vec![]);
        }
        let mut out = Vec::new();
        for e in std::fs::read_dir(&base)?.filter_map(|e| e.ok()) {
            if !e.path().is_dir() {
                continue;
            }
            if let Ok(Some(p)) = load_project(&e.path()) {
                out.push(p);
            }
        }
        out.sort_by(|a, b| b.meta.updated_at.cmp(&a.meta.updated_at));
        Ok(out)
    }

    /// 创建(或复用)工程目录。
    ///
    /// **同一音频重复处理会复用同一个工程** —— 因为 `id` 就是内容哈希。
    /// 这条很重要:重跑一次不该产生两个工程。
    pub fn create_or_open(&self, id: &str, title: &str) -> Result<Project> {
        if let Some(p) = self.find(id)? {
            return Ok(p);
        }

        let dir = self.unique_dir(title)?;
        std::fs::create_dir_all(dir.join(AUDIO_DIR))
            .with_context(|| format!("创建工程目录失败: {}", dir.display()))?;

        let now = crate::store::db::now_ms();
        let meta = ProjectMeta {
            version: 1,
            id: id.to_string(),
            title: title.to_string(),
            slug: dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| title.to_string()),
            created_at: now,
            updated_at: now,
            duration_ms: 0,
            scene: None,
            scene_confidence: None,
            source_path: None,
            asr_model: None,
            asr_backend: None,
            has_speakers: false,
            speaker_count: 0,
            artifacts: Artifacts::default(),
        };
        let p = Project { dir, meta };
        p.save_meta()?;
        Ok(p)
    }

    /// 生成不冲突的目录名:`<日期>_<标题>`,重名时追加 `_2` `_3`。
    fn unique_dir(&self, title: &str) -> Result<PathBuf> {
        let base = self.projects_dir();
        std::fs::create_dir_all(&base)?;

        let slug = slugify(title, 60);
        let date = date_prefix();
        let stem = format!("{date}_{slug}");
        Ok(self.unique_in(&base, &stem, None))
    }

    /// 在 `base` 下找一个不冲突的目录名。
    ///
    /// `avoid` 用于重命名:原目录本身不算冲突(它会被移走)。
    fn unique_in(&self, base: &Path, stem: &str, avoid: Option<&Path>) -> PathBuf {
        let is_free = |p: &Path| -> bool {
            if let Some(a) = avoid {
                if p == a {
                    return true;
                }
            }
            !p.exists()
        };

        let mut cand = base.join(stem);
        let mut n = 2;
        while !is_free(&cand) {
            cand = base.join(format!("{stem}_{n}"));
            n += 1;
            if n > 999 {
                break;
            }
        }
        cand
    }

    /// 重命名工程:**标题与目录名一起改**。
    ///
    /// # 为什么两个都改
    ///
    /// 工程目录是给人看的(`2026-09-11_9月11日 复兴路`),标题是界面上显示的。
    /// 只改一个会让两者长期不一致 —— 用户在资源管理器里看到的和界面上看到的
    /// 是两回事,反而更难找。
    ///
    /// # 保留日期前缀
    ///
    /// 目录名的 `<日期>_` 前缀是**创建日期**,不是"最后修改日期"。
    /// 重命名时保留它 —— 否则用户重命名一次,这个工程的时间线就乱了,
    /// 按时间排序会跳到最前面。
    ///
    /// # 对同步的影响
    ///
    /// **目录名变了,云端路径也全变。** 旧路径下的文件会变成孤儿。
    /// 这里只做本地改名,并把"旧路径"返回给调用方,由它提醒用户 ——
    /// 删除云端是 [`crate::sync`] 的事,而它有自己的安全规则
    /// (范围、方向、删除策略),不该被一个重命名绕过去。
    pub fn rename(&self, id_prefix: &str, new_title: &str) -> Result<RenameOutcome> {
        let new_title = new_title.trim();
        if new_title.is_empty() {
            anyhow::bail!("新标题不能为空");
        }
        let p = self
            .find(id_prefix)?
            .ok_or_else(|| anyhow!("找不到工程: {id_prefix}"))?;

        let old_dir = p.dir.clone();
        let old_slug = p.meta.slug.clone();

        // 保留原目录名的日期前缀;取不到就用今天
        let date = slug_date_prefix(&old_slug).unwrap_or_else(date_prefix);
        let stem = format!("{date}_{}", slugify(new_title, 60));

        let base = self.projects_dir();
        let target = self.unique_in(&base, &stem, Some(&old_dir));

        if target == old_dir {
            // 名字没变(或只差非法字符),只更新标题
            let mut m = p.meta.clone();
            m.title = new_title.to_string();
            m.updated_at = crate::store::db::now_ms();
            let np = Project {
                dir: old_dir.clone(),
                meta: m,
            };
            np.save_meta()?;
            return Ok(RenameOutcome {
                dir: old_dir,
                full_id: p.meta.id.clone(),
                old_slug,
                slug_changed: false,
                warning: None,
            });
        }

        // 先移动目录,再把新元数据写进去 ——
        // 顺序反过来的话,移动失败会留下一个"标题已改但目录没动"的中间态。
        std::fs::rename(&old_dir, &target)
            .with_context(|| format!("重命名目录失败: {} -> {}", old_dir.display(), target.display()))?;

        let mut m = p.meta.clone();
        m.title = new_title.to_string();
        m.slug = target
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| stem.clone());
        m.updated_at = crate::store::db::now_ms();

        let np = Project {
            dir: target.clone(),
            meta: m,
        };
        if let Err(e) = np.save_meta() {
            // 元数据写不进去就把目录移回去,别留下半截状态
            let _ = std::fs::rename(&target, &old_dir);
            return Err(e).context("写入新元数据失败,已回滚目录改名");
        }

        Ok(RenameOutcome {
            dir: target,
            full_id: p.meta.id.clone(),
            old_slug,
            slug_changed: true,
            warning: None,
        })
    }
}

/// 按目录找工程。
///
/// # 为什么删除必须按目录而不是按 ID
///
/// `find()` 按 ID 前缀查找。但**同一个音频可以建多个工程** ——
/// 处理两次就会出现两个 ID 相同、目录不同的工程。那时 `find()` 会
/// 报"匹配到多个工程"而拒绝,删除就没法进行。
///
/// 目录是唯一的,所以删除按目录定位。ID 前缀只用于"帮用户找到目录"。
pub fn load_project_dir(dir: &Path) -> Result<Option<Project>> {
    if !dir.is_dir() {
        return Ok(None);
    }
    load_project(dir)
}

/// 删除一个工程时,哪些东西被删了、哪些没动。
///
/// 每一项都写清楚,是因为"删除"这个动作最怕含糊 ——
/// 用户需要知道云端还在不在、原始录音还在不在。
#[derive(Clone, Debug, Default)]
pub struct DeleteOutcome {
    /// 工程 ID
    pub id: String,
    /// 被删的工程标题
    pub title: String,
    /// 被删的工程目录
    pub dir: PathBuf,
    /// 工程目录占用的字节数
    pub bytes_freed: u64,
    /// 目录里的文件数
    pub files_deleted: usize,
    /// 一并删掉的按哈希命名的内容文件(transcript/summary/labels)
    pub content_files_deleted: Vec<PathBuf>,
    /// **没删**的内容文件。有别的工程用同一个音频时会出现 ——
    /// 那些文件是共享的,删了会连带毁掉另一个工程。
    pub content_files_kept: Vec<PathBuf>,
    /// 原始录音路径(如果会话表里有记录)
    pub source_audio: Option<PathBuf>,
    /// 原始录音是否真的被删了
    pub source_deleted: bool,
}

impl DeleteOutcome {
    /// 这个工程在同步清单里有没有记录。
    ///
    /// 有就说明云端可能还留着一份,**调用方应当提醒用户** ——
    /// 删除只在本地生效,云端要另外清(「云端管理」里有删除按钮)。
    pub fn had_cloud_copy(&self, manifest: &crate::sync::manifest::Manifest) -> bool {
        let needle = format!("/{}/", self.id);
        manifest
            .live_files()
            .any(|(k, _)| k.starts_with("projects/") && k.contains(&needle))
    }
}

/// 删除一个工程。
///
/// `dir` 是**工程目录**(不是 ID 前缀)—— 见 [`load_project_dir`] 里
/// 关于"同 ID 多工程"的说明。
///
/// # 删什么
///
/// 1. **工程目录**(含里面的音频副本)—— 这是主体
/// 2. **按内容哈希命名的内容文件**(transcript / summary / speaker_labels)
///
/// # 不删什么,以及为什么
///
/// - **历史记录(sessions 表)不动。** 用户明确要求保留 —— 它记录了
///   "这段录音处理过",工程只是它的一个视图。删掉工程后,历史记录里
///   那条会显示为"工程已删除"。
/// - **云端副本不动。** 删除只在本地生效。云端的清理是用户显式动作
///   (「云端管理」里有删除按钮),自动删云端会让"手滑删本地"变成
///   "云端也没了"。调用方应当在结果里提醒用户。
/// - **声纹档案不动。** 它是全局的,跨会话存在,不属于某一个工程。
///
/// # 共享内容文件的保护
///
/// 内容文件按**音频内容哈希**命名,所以同一个音频只存一份。
/// 删除前会检查 store 里是否还有**别的工程**用同一个 ID;
/// 有的话那些文件保留,记进 `content_files_kept`。
///
/// # 原始录音
///
/// 默认**不动**。它在应用的存储目录之外(用户的 Downloads 之类),
/// 删除别人的文件不该是默认行为。要删的话由调用方显式传 `delete_source`。
pub fn delete_project(
    store: &ProjectStore,
    files: &crate::store::files::FileStore,
    db: &crate::store::db::Db,
    dir: &Path,
    delete_source: bool,
) -> Result<DeleteOutcome> {
    let p = load_project_dir(dir)?
        .ok_or_else(|| anyhow!("找不到工程目录: {}", dir.display()))?;

    let id = p.meta.id.clone();
    let title = p.meta.title.clone();
    let dir = p.dir.clone();

    let mut out = DeleteOutcome {
        id: id.clone(),
        title,
        dir: dir.clone(),
        ..Default::default()
    };

    // ---- 1. 内容文件(先判断共享)----
    //
    // 必须**在删目录之前**检查:别的工程是否也用这个 ID。
    // 用同一个音频建的第二个工程会共享这些文件。
    let others_share = store.list()?.into_iter().any(|q| q.meta.id == id && q.dir != dir);

    let content: Vec<PathBuf> = [
        files.transcript_path(&id),
        files.summary_path(&id),
        // meta 是 summary.md 的旁挂文件(write_summary 里用 with_extension)
        files.summary_path(&id).with_extension("meta.json"),
        files.labels_path(&id),
    ]
    .into_iter()
    .filter(|f| f.exists())
    .collect();

    if others_share {
        out.content_files_kept = content;
    } else {
        for f in content {
            match std::fs::remove_file(&f) {
                Ok(()) => out.content_files_deleted.push(f),
                // 删不掉不算失败 —— 记下来让调用方报告,别让整次删除中断
                Err(e) => {
                    tracing::warn!("删除内容文件失败 {}: {e}", f.display());
                    out.content_files_kept.push(f);
                }
            }
        }
        // 清掉可能空掉的分片目录
        prune_empty_shards(files, &id);
    }

    // ---- 2. 工程目录 ----
    let (files_n, bytes) = dir_stats(&dir);
    out.files_deleted = files_n;
    out.bytes_freed = bytes;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("删除工程目录失败: {}", dir.display()))?;
    }

    // ---- 3. 原始录音(可选)----
    out.source_audio = db
        .get_session(&id)
        .ok()
        .flatten()
        .and_then(|s| s.audio_local_path)
        .map(PathBuf::from);

    if delete_source {
        if let Some(src) = &out.source_audio {
            // 只删文件,不递归 —— 万一记的是个目录,不动它
            if src.is_file() {
                match std::fs::remove_file(src) {
                    Ok(()) => out.source_deleted = true,
                    Err(e) => tracing::warn!("删除原始录音失败 {}: {e}", src.display()),
                }
            }
        }
    }

    Ok(out)
}

/// 内容文件删完后,把空掉的分片目录(`transcript/1f/` 之类)也清掉。
///
/// 失败**静默忽略** —— 留个空目录不影响任何功能,不值得为它报错。
fn prune_empty_shards(files: &crate::store::files::FileStore, id: &str) {
    let shard = crate::types::hash_shard(id);
    if shard.is_empty() {
        return;
    }
    for kind in ["transcript", "summary", "speaker_labels"] {
        let d = files.root().join(kind).join(shard);
        if d.is_dir()
            && std::fs::read_dir(&d)
                .map(|mut i| i.next().is_none())
                .unwrap_or(false)
        {
            let _ = std::fs::remove_dir(&d);
        }
    }
}

/// 统计一个工程目录的文件数与字节数。
///
/// 也用于删除前的确认("将要释放 148.5 MB")。
pub fn project_stats(dir: &Path) -> (usize, u64) {
    dir_stats(dir)
}

/// 递归统计目录里的文件数与总字节数。
///
/// 目录不存在时返回 `(0, 0)` —— 调用方在删之前想知道"能腾出多少",
/// 不该因为目录已经不在就报错。
fn dir_stats(dir: &Path) -> (usize, u64) {
    let mut n = 0usize;
    let mut bytes = 0u64;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    for e in rd.filter_map(|e| e.ok()) {
        match e.file_type() {
            Ok(t) if t.is_dir() => {
                let (cn, cb) = dir_stats(&e.path());
                n += cn;
                bytes += cb;
            }
            Ok(t) if t.is_file() => {
                n += 1;
                bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
            _ => {}
        }
    }
    (n, bytes)
}

/// 重命名工程,**并同步会话表里的标题**。
///
/// # 为什么要有这个函数(而不是让调用方各写一遍)
///
/// 标题在两处各存一份:
///
/// ```text
/// projects/<目录>/project.json 的 title   →  「我的工程」读它
/// sessions.title                          →  「历史记录」「处理录音」读它
/// ```
///
/// 两边用同一个 ID(音频内容哈希)关联,但是两套独立存储。
/// **只改一处,同一个录音就会在两个页面显示不同的名字。**
///
/// 最初 CLI 和 GUI 各写了一遍改名逻辑,GUI 那份同步了会话表、CLI 那份漏了 ——
/// 于是"用什么入口改名"决定了标题会不会一致。抽成一个函数就没有这个空间。
///
/// # 数据库失败不算改名失败
///
/// 工程才是真相源。会话表只是索引,它没跟上最多是历史记录显示旧名字,
/// 下次改名或重建索引就能补回来。所以这里返回 `title_sync_warning`
/// 让调用方去提示,而不是把整次改名判为失败。
pub fn rename_project_synced(
    store: &ProjectStore,
    db: &crate::store::db::Db,
    id_prefix: &str,
    new_title: &str,
) -> Result<RenameOutcome> {
    let mut out = store.rename(id_prefix, new_title)?;

    match db.set_session_title(&out.full_id, new_title.trim()) {
        Ok(()) => {}
        Err(e) => {
            tracing::warn!("更新会话标题失败(历史记录可能显示旧名字): {e}");
            out.warning = Some(format!("历史记录里的名字没更新:{e}"));
        }
    }
    Ok(out)
}

/// 重命名的结果。
#[derive(Clone, Debug)]
pub struct RenameOutcome {
    /// 改名后的目录
    pub dir: PathBuf,
    /// 工程 ID(音频内容哈希)。调用方需要它来同步会话表标题 ——
    /// 标题在工程与会话表两处各有一份,靠这个 ID 关联。
    pub full_id: String,
    /// 原来的目录名(用于提醒云端路径已变)
    pub old_slug: String,
    /// 目录名是否真的变了
    pub slug_changed: bool,
    /// 需要提醒用户的事
    pub warning: Option<String>,
}

/// 从 `<YYYY-MM-DD>_<标题>` 里取出日期部分。
fn slug_date_prefix(slug: &str) -> Option<String> {
    let (head, _) = slug.split_once('_')?;
    let b = head.as_bytes();
    if b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(|c| c.is_ascii_digit())
        && b[5..7].iter().all(|c| c.is_ascii_digit())
        && b[8..10].iter().all(|c| c.is_ascii_digit())
    {
        Some(head.to_string())
    } else {
        None
    }
}

/// 日期前缀 `YYYY-MM-DD`(本地时间)。
fn date_prefix() -> String {
    // 不引 chrono:只需要一个日期串,用系统时间手工算
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 从 Unix 纪元按天推算
    let days = now / 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// 天数 → (年, 月, 日)。Howard Hinnant 的算法,无闰年特判分支。
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

/// 从磁盘读一个工程目录。
pub fn load_project(dir: &Path) -> Result<Option<Project>> {
    let meta_path = dir.join(META_JSON);
    if !meta_path.is_file() {
        return Ok(None);
    }
    let meta: ProjectMeta = crate::store::files::read_json(&meta_path)?
        .ok_or_else(|| anyhow!("工程元数据读取失败: {}", meta_path.display()))?;
    Ok(Some(Project {
        dir: dir.to_path_buf(),
        meta,
    }))
}

/// 把一份音频拷进工程目录。
///
/// 返回目标路径。**已存在且大小一致时跳过** —— 重复处理同一录音不该反复拷贝。
pub fn copy_audio(project_dir: &Path, src: &Path, content_hash: &str) -> Result<PathBuf> {
    let adir = project_dir.join(AUDIO_DIR);
    std::fs::create_dir_all(&adir)?;

    let ext = src
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_else(|| "bin".into());
    let dst = adir.join(format!("recording.{ext}"));

    let need_copy = match (std::fs::metadata(&dst), std::fs::metadata(src)) {
        (Ok(a), Ok(b)) => a.len() != b.len(),
        _ => true,
    };
    if need_copy {
        std::fs::copy(src, &dst)
            .with_context(|| format!("拷贝录音失败: {} -> {}", src.display(), dst.display()))?;
    }

    // 记一份哈希,便于去重与校验
    std::fs::write(adir.join("audio.sha256"), content_hash.as_bytes())?;
    Ok(dst)
}

/// 写入工程的全部文本产物。
///
/// 分成两步是刻意的:**转写先落盘**(用户马上能看),总结后补。
pub fn write_transcript_artifacts(
    project_dir: &Path,
    transcript: &Transcript,
    labels: &SpeakerLabels,
) -> Result<()> {
    use crate::pipeline::view;

    std::fs::write(
        project_dir.join(TRANSCRIPT_MD),
        view::render_dialogue(&transcript.segments, labels),
    )?;
    std::fs::write(
        project_dir.join(TRANSCRIPT_SRT),
        view::render_srt(&transcript.segments, labels),
    )?;
    crate::store::files::write_json(&project_dir.join(TRANSCRIPT_JSON), transcript)?;
    Ok(())
}

pub fn write_summary(project_dir: &Path, kind: SummaryKind, summary: &Summary) -> Result<()> {
    std::fs::write(project_dir.join(kind.file_name()), &summary.content_md)?;
    Ok(())
}

pub fn write_mindmap(project_dir: &Path, mindmap: &crate::types::MindMap) -> Result<()> {
    std::fs::write(project_dir.join(MINDMAP), &mindmap.mermaid)?;
    // 文本大纲作为降级产物一并留下 —— 图渲染失败时它就是备份
    if !mindmap.outline.trim().is_empty() {
        std::fs::write(project_dir.join("mindmap-outline.md"), &mindmap.outline)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Backend, Segment, SpeakerLabels, TokenUsage};

    fn fixture() -> (tempfile::TempDir, ProjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStore::new(dir.path());
        (dir, store)
    }

    fn sample_transcript() -> Transcript {
        Transcript {
            engine: "mock".into(),
            model: "m".into(),
            backend: Backend::Cpu,
            backend_diarize: None,
            language: Some("zh".into()),
            segments: vec![Segment::new(0, 1000, "内容")],
            raw_text: "内容".into(),
            duration_ms: 1000,
            diarize: None,
        }
    }

    fn sample_summary(kind: &str) -> Summary {
        Summary {
            content_md: format!("# {kind}"),
            scene: Scene::Lecture,
            model: "m".into(),
            usage: TokenUsage::default(),
            labels_version: 0,
            used_map_reduce: false,
        }
    }

    // --- slugify -----------------------------------------------------------

    #[test]
    fn slugify_removes_path_illegal_chars() {
        assert_eq!(slugify("a/b\\c:d*e?f", 60), "a b c d e f");
        assert!(!slugify("a<b>c|d\"e", 60).contains(['<', '>', '|', '"']));
    }

    #[test]
    fn slugify_collapses_whitespace() {
        assert_eq!(slugify("高等数学    第 12  讲", 60), "高等数学 第 12 讲");
        assert_eq!(slugify("  前后有空格  ", 60), "前后有空格");
    }

    #[test]
    fn slugify_truncates_by_chars_not_bytes() {
        // 中文按字符截断,不能截出半个字导致乱码
        let long = "讲".repeat(200);
        let s = slugify(&long, 10);
        assert_eq!(s.chars().count(), 10);
        assert!(s.is_char_boundary(s.len()));
    }

    #[test]
    fn slugify_falls_back_when_empty() {
        assert_eq!(slugify("", 60), "未命名录音");
        assert_eq!(slugify("///", 60), "未命名录音");
        assert_eq!(slugify("   ", 60), "未命名录音");
    }

    #[test]
    fn slugify_strips_trailing_dot() {
        // Windows 不允许目录名以点结尾
        assert_eq!(slugify("abc.", 60), "abc");
        assert_eq!(slugify("abc...", 60), "abc");
    }

    // --- 目录名 ------------------------------------------------------------

    #[test]
    fn unique_dir_appends_counter_on_conflict() {
        let (_d, store) = fixture();
        let a = store.unique_dir("同一节课").unwrap();
        std::fs::create_dir_all(&a).unwrap();
        let b = store.unique_dir("同一节课").unwrap();
        assert_ne!(a, b, "同名工程应得到不同目录");
        assert!(b.to_string_lossy().ends_with("_2"));
    }

    #[test]
    fn unique_dir_has_date_prefix() {
        let (_d, store) = fixture();
        let p = store.unique_dir("测试").unwrap();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        // 形如 2026-09-11_测试
        assert_eq!(name.chars().nth(4), Some('-'));
        assert_eq!(name.chars().nth(7), Some('-'));
        assert!(name.contains('_'));
    }

    // --- 工程生命周期 ------------------------------------------------------

    #[test]
    fn create_open_and_reopen() {
        let (_d, store) = fixture();
        let p = store.create_or_open("hash_a", "高等数学第12讲").unwrap();
        assert!(p.dir.is_dir());
        assert!(p.dir.join(AUDIO_DIR).is_dir(), "应建 audio 子目录");
        assert!(p.dir.join(META_JSON).is_file());

        let again = store.create_or_open("hash_a", "高等数学第12讲").unwrap();
        assert_eq!(p.dir, again.dir, "★ 同一音频必须复用同一工程");
    }

    #[test]
    fn find_by_id_and_prefix() {
        let (_d, store) = fixture();
        let p = store.create_or_open("abcdef123456", "T").unwrap();
        assert!(store.find("abcdef123456").unwrap().is_some());
        assert!(store.find("abcdef").unwrap().is_some(), "应支持前缀匹配");
        assert!(store.find("zzz").unwrap().is_none());
    }

    #[test]
    fn list_returns_projects_newest_first() {
        let (_d, store) = fixture();
        let a = store.create_or_open("h1", "第一个").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut b = store.create_or_open("h2", "第二个").unwrap();
        b.meta.updated_at = a.meta.updated_at + 10_000;
        b.save_meta().unwrap();

        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].meta.id, "h2", "应按更新时间倒序");
    }

    #[test]
    fn list_on_empty_returns_empty() {
        let (_d, store) = fixture();
        assert!(store.list().unwrap().is_empty());
    }

    // --- 产物 --------------------------------------------------------------

    #[test]
    fn copy_audio_is_idempotent() {
        let (_d, store) = fixture();
        let p = store.create_or_open("h", "T").unwrap();

        let src = _d.path().join("in.m4a");
        std::fs::write(&src, b"fake audio bytes").unwrap();

        let d1 = copy_audio(&p.dir, &src, "hash1").unwrap();
        assert!(d1.is_file());
        assert_eq!(d1.file_name().unwrap(), "recording.m4a");
        let m1 = std::fs::metadata(&d1).unwrap().modified().unwrap();

        // 再拷一次(同大小)→ 应跳过,不重写文件
        std::thread::sleep(std::time::Duration::from_millis(20));
        let d2 = copy_audio(&p.dir, &src, "hash1").unwrap();
        assert_eq!(d1, d2);
        let m2 = std::fs::metadata(&d2).unwrap().modified().unwrap();
        assert_eq!(m1, m2, "★ 内容未变时不该重复拷贝");

        // 哈希文件应存在
        assert!(p.dir.join(AUDIO_DIR).join("audio.sha256").is_file());
    }

    #[test]
    fn audio_path_skips_hash_file() {
        let (_d, store) = fixture();
        let p = store.create_or_open("h", "T").unwrap();
        let src = _d.path().join("x.wav");
        std::fs::write(&src, b"abc").unwrap();
        copy_audio(&p.dir, &src, "h").unwrap();

        let found = audio_path(&p.dir).expect("应能找到录音");
        assert_eq!(found.file_name().unwrap(), "recording.wav");
        assert_ne!(
            found.file_name().unwrap().to_string_lossy(),
            "audio.sha256",
            "★ 校验文件不能被当成录音"
        );
    }

    #[test]
    fn write_all_transcript_artifacts() {
        let (_d, store) = fixture();
        let p = store.create_or_open("h", "T").unwrap();
        let mut labels = SpeakerLabels::new("h");
        labels.ensure([0]);

        write_transcript_artifacts(&p.dir, &sample_transcript(), &labels).unwrap();

        assert!(p.dir.join(TRANSCRIPT_MD).is_file());
        assert!(p.dir.join(TRANSCRIPT_JSON).is_file());
        assert!(p.dir.join(TRANSCRIPT_SRT).is_file());

        let md = std::fs::read_to_string(p.dir.join(TRANSCRIPT_MD)).unwrap();
        assert!(md.contains("内容"));

        let srt = std::fs::read_to_string(p.dir.join(TRANSCRIPT_SRT)).unwrap();
        assert!(srt.contains("-->"), "SRT 必须有时间轴");
    }

    #[test]
    fn write_both_summaries() {
        let (_d, store) = fixture();
        let p = store.create_or_open("h", "T").unwrap();

        write_summary(&p.dir, SummaryKind::Detailed, &sample_summary("详细")).unwrap();
        write_summary(&p.dir, SummaryKind::Brief, &sample_summary("简略")).unwrap();

        assert_eq!(
            p.read_summary(SummaryKind::Detailed).unwrap(),
            "# 详细"
        );
        assert_eq!(p.read_summary(SummaryKind::Brief).unwrap(), "# 简略");
    }

    #[test]
    fn write_mindmap_also_saves_outline_fallback() {
        let (_d, store) = fixture();
        let p = store.create_or_open("h", "T").unwrap();
        let mm = crate::types::MindMap::new("mindmap\n  root((主题))\n    要点A");

        write_mindmap(&p.dir, &mm).unwrap();

        assert!(p.dir.join(MINDMAP).is_file());
        let saved = p.read_mindmap().unwrap();
        assert!(saved.starts_with("mindmap"));

        // 大纲也要留下 —— 图渲染失败时它是备份
        if !mm.outline.trim().is_empty() {
            assert!(p.dir.join("mindmap-outline.md").is_file());
        }
    }

    #[test]
    fn artifacts_detection_reflects_reality() {
        let (_d, store) = fixture();
        let p = store.create_or_open("h", "T").unwrap();

        let a = Artifacts::detect(&p.dir);
        assert!(!a.transcript && !a.summary_detailed && !a.mindmap);
        assert!(a.missing().contains(&"转写"));
        assert!(a.missing().contains(&"详细总结"));

        let mut labels = SpeakerLabels::new("h");
        labels.ensure([0]);
        write_transcript_artifacts(&p.dir, &sample_transcript(), &labels).unwrap();
        write_summary(&p.dir, SummaryKind::Detailed, &sample_summary("d")).unwrap();

        let b = Artifacts::detect(&p.dir);
        assert!(b.transcript && b.transcript_json && b.transcript_srt);
        assert!(b.summary_detailed);
        assert!(!b.summary_brief, "还差简略总结");
        assert!(b.missing().contains(&"简略总结"));
    }

    #[test]
    fn refresh_artifacts_updates_meta() {
        let (_d, store) = fixture();
        let mut p = store.create_or_open("h", "T").unwrap();
        assert!(!p.meta.artifacts.transcript);

        let mut labels = SpeakerLabels::new("h");
        labels.ensure([0]);
        write_transcript_artifacts(&p.dir, &sample_transcript(), &labels).unwrap();
        p.refresh_artifacts().unwrap();

        assert!(p.meta.artifacts.transcript);
        // 重新从磁盘读也应看到
        let reloaded = load_project(&p.dir).unwrap().unwrap();
        assert!(reloaded.meta.artifacts.transcript);
    }

    #[test]
    fn legacy_dir_without_meta_is_ignored() {
        let (_d, store) = fixture();
        std::fs::create_dir_all(store.projects_dir().join("随便一个目录")).unwrap();
        assert!(store.list().unwrap().is_empty(), "没有 project.json 的目录应被忽略");
    }

    #[test]
    fn summary_kind_parse_and_file_names() {
        assert_eq!(SummaryKind::parse("detailed"), Some(SummaryKind::Detailed));
        assert_eq!(SummaryKind::parse("详细"), Some(SummaryKind::Detailed));
        assert_eq!(SummaryKind::parse("brief"), Some(SummaryKind::Brief));
        assert_eq!(SummaryKind::parse("简略"), Some(SummaryKind::Brief));
        assert_eq!(SummaryKind::parse("nope"), None);
        assert_ne!(
            SummaryKind::Detailed.file_name(),
            SummaryKind::Brief.file_name()
        );
    }

    #[test]
    fn civil_from_days_known_dates() {
        // 1970-01-01
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-03-01(闰年边界附近)
        assert_eq!(civil_from_days(11017), (2000, 3, 1));
    }

    // --- 删除工程 ----------------------------------------------------------

    /// 测试用的一整套 store + files + db。
    fn del_fixture() -> (
        tempfile::TempDir,
        ProjectStore,
        crate::store::files::FileStore,
        crate::store::db::Db,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let files = crate::store::files::FileStore::new(dir.path().join("store"));
        files.ensure_dirs().unwrap();
        let db = crate::store::db::Db::open(&dir.path().join("store").join("cache.db")).unwrap();
        let store = ProjectStore::new(files.root());
        (dir, store, files, db)
    }

    #[test]
    fn delete_removes_project_dir_and_content_files() {
        let (_d, store, files, db) = del_fixture();
        let p = store.create_or_open("hash_del", "待删除").unwrap();
        std::fs::write(p.dir.join(TRANSCRIPT_MD), "转写").unwrap();
        // 也放一份按哈希命名的产物
        let tp = files.transcript_path("hash_del");
        std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
        std::fs::write(&tp, "{}").unwrap();

        let out = delete_project(&store, &files, &db, &p.dir, false).unwrap();

        assert!(!out.dir.exists(), "工程目录应已删除");
        assert!(!tp.exists(), "按哈希命名的产物也应删除");
        assert!(out.bytes_freed > 0, "应统计出释放的字节数");
        assert!(out.files_deleted > 0);
        assert!(store.find("hash_del").unwrap().is_none(), "应查不到这个工程了");
    }

    #[test]
    fn delete_keeps_session_record() {
        // ★ 用户明确要求:删工程保留历史记录 ——
        //   它记录了"这段录音处理过",工程只是它的一个视图。
        let (_d, store, files, db) = del_fixture();
        let mut row = crate::store::db::SessionRow::new_for_test("hash_keep_sess");
        row.title = Some("留着".into());
        db.upsert_session(&row).unwrap();

        let p = store.create_or_open("hash_keep_sess", "工程").unwrap();
        delete_project(&store, &files, &db, &p.dir, false).unwrap();

        let s = db.get_session("hash_keep_sess").unwrap();
        assert!(s.is_some(), "★ 会话记录必须保留");
        assert_eq!(s.unwrap().title.as_deref(), Some("留着"));
    }

    #[test]
    fn delete_keeps_source_audio_by_default() {
        // ★ 原始录音在应用目录之外,默认不碰
        let (_d, store, files, db) = del_fixture();
        let src = _d.path().join("外部录音.m4a");
        std::fs::write(&src, b"audio bytes").unwrap();

        let mut row = crate::store::db::SessionRow::new_for_test("hash_src");
        row.audio_local_path = Some(src.to_string_lossy().to_string());
        db.upsert_session(&row).unwrap();
        let p = store.create_or_open("hash_src", "工程").unwrap();

        let out = delete_project(&store, &files, &db, &p.dir, false).unwrap();

        assert!(src.is_file(), "★ 默认不删原始录音");
        assert!(!out.source_deleted);
        assert_eq!(out.source_audio.as_deref(), Some(src.as_path()));
    }

    #[test]
    fn delete_removes_source_audio_when_asked() {
        let (_d, store, files, db) = del_fixture();
        let src = _d.path().join("要删的录音.m4a");
        std::fs::write(&src, b"audio bytes").unwrap();

        let mut row = crate::store::db::SessionRow::new_for_test("hash_src2");
        row.audio_local_path = Some(src.to_string_lossy().to_string());
        db.upsert_session(&row).unwrap();
        let p = store.create_or_open("hash_src2", "工程").unwrap();

        let out = delete_project(&store, &files, &db, &p.dir, true).unwrap();

        assert!(!src.exists(), "显式要求时应删掉原始录音");
        assert!(out.source_deleted);
    }

    #[test]
    fn delete_keeps_content_files_shared_with_another_project() {
        // ★ 内容文件按**音频内容哈希**命名,同一个音频只存一份。
        //   用同一个音频建的第二个工程会共享它 —— 删一个不能连带毁掉另一个。
        let (_d, store, files, db) = del_fixture();
        let a = store.create_or_open("same_hash", "第一讲").unwrap();
        // 手工造一个同 ID 的第二个工程(create_or_open 遇到同 ID 会复用)
        let b_dir = store.projects_dir().join("2026-01-02_第二讲");
        std::fs::create_dir_all(b_dir.join(AUDIO_DIR)).unwrap();
        let mut meta = a.meta.clone();
        meta.slug = "2026-01-02_第二讲".into();
        meta.title = "第二讲".into();
        Project {
            dir: b_dir.clone(),
            meta,
        }
        .save_meta()
        .unwrap();

        let tp = files.transcript_path("same_hash");
        std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
        std::fs::write(&tp, "{}").unwrap();

        let out = delete_project(&store, &files, &db, &a.dir, false).unwrap();

        assert!(!out.dir.exists(), "被删的那个工程目录应消失");
        assert!(b_dir.is_dir(), "★ 同 ID 的另一个工程必须完好");
        assert!(tp.is_file(), "★ 共享的产物文件必须保留");
        assert_eq!(out.content_files_kept.len(), 1);
        assert!(out.content_files_deleted.is_empty());
    }

    #[test]
    fn delete_unshares_after_the_other_project_is_gone() {
        // 只剩一个工程时,产物文件就该删掉了
        let (_d, store, files, db) = del_fixture();
        let p = store.create_or_open("h", "唯一").unwrap();
        let tp = files.transcript_path("h");
        std::fs::create_dir_all(tp.parent().unwrap()).unwrap();
        std::fs::write(&tp, "{}").unwrap();

        let out = delete_project(&store, &files, &db, &p.dir, false).unwrap();
        assert!(out.content_files_kept.is_empty());
        assert_eq!(out.content_files_deleted.len(), 1);
        assert!(!tp.exists());
    }

    #[test]
    fn delete_prunes_empty_shard_dirs() {
        let (_d, store, files, db) = del_fixture();
        let p = store.create_or_open("h", "T").unwrap();
        let tp = files.transcript_path("h");
        let shard_dir = tp.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&shard_dir).unwrap();
        std::fs::write(&tp, "{}").unwrap();

        delete_project(&store, &files, &db, &p.dir, false).unwrap();
        assert!(!shard_dir.exists(), "空掉的分片目录应被清掉");
    }

    #[test]
    fn delete_rejects_unknown_project() {
        let (_d, store, files, db) = del_fixture();
        let e = delete_project(&store, &files, &db, std::path::Path::new("nope"), false).unwrap_err();
        assert!(e.to_string().contains("找不到"), "{e}");
    }

    #[test]
    fn delete_supports_id_prefix() {
        let (_d, store, files, db) = del_fixture();
        let p = store.create_or_open("abcdef123456", "前缀").unwrap();
        let out = delete_project(&store, &files, &db, &p.dir, false).unwrap();
        assert_eq!(out.id, "abcdef123456");
    }

    #[test]
    fn delete_reports_cloud_copy_from_manifest() {
        // 有同步记录时调用方要提醒"云端还在"
        use crate::sync::manifest::{Manifest, ManifestEntry};
        let (_d, store, files, db) = del_fixture();
        let p = store.create_or_open("cid", "云端有").unwrap();
        let out = delete_project(&store, &files, &db, &p.dir, false).unwrap();

        let mut m = Manifest::new();
        assert!(!out.had_cloud_copy(&m), "空清单 → 没有云端副本");

        m.record(
            "projects/2026-01-01_x/audio/recording.m4a",
            ManifestEntry {
                etag: Some("\"e\"".into()),
                size: Some(1),
                synced_at: 1,
                origin: Some("dev".into()),
                deleted_at: None,
            },
        );
        // 路径里不含这个 ID → 仍算没有
        assert!(!out.had_cloud_copy(&m));

        m.record(
            &format!("projects/2026-01-01_x/{}/audio.m4a", out.id),
            ManifestEntry {
                etag: Some("\"e\"".into()),
                size: Some(1),
                synced_at: 1,
                origin: Some("dev".into()),
                deleted_at: None,
            },
        );
        assert!(out.had_cloud_copy(&m), "★ 路径含这个 ID 时才算云端有副本");
    }

    #[test]
    fn project_stats_counts_files_and_bytes() {
        let (_d, store) = fixture();
        let p = store.create_or_open("h", "统计").unwrap();
        std::fs::write(p.dir.join("a.txt"), "12345").unwrap();
        std::fs::write(p.dir.join(AUDIO_DIR).join("b.txt"), "123").unwrap();

        // create_or_open 会写一个 project.json,所以总共 3 个文件
        let meta_bytes = std::fs::metadata(p.dir.join(META_JSON)).unwrap().len();
        let (n, bytes) = project_stats(&p.dir);
        assert_eq!(n, 3, "a.txt + audio/b.txt + project.json");
        assert_eq!(bytes, 5 + 3 + meta_bytes);
    }

    #[test]
    fn dir_stats_on_missing_dir_is_zero() {
        let (n, b) = dir_stats(std::path::Path::new("D:\\definitely\\not\\here"));
        assert_eq!((n, b), (0, 0));
    }

    #[test]
    fn date_prefix_shape() {
        let d = date_prefix();
        assert_eq!(d.len(), 10, "{d}");
        assert_eq!(&d[4..5], "-");
        assert_eq!(&d[7..8], "-");
    }

    // --- 重命名 ------------------------------------------------------------

    #[test]
    fn rename_changes_title_and_directory() {
        let (_d, store) = fixture();
        let p = store.create_or_open("hash_a", "9月11日 复兴路").unwrap();
        let old_dir = p.dir.clone();
        assert!(old_dir.to_string_lossy().contains("9月11日 复兴路"));

        let out = store.rename("hash_a", "图像处理第一讲").unwrap();
        assert!(out.slug_changed);
        assert!(old_dir.exists() == false, "旧目录应已移走");
        assert!(out.dir.is_dir(), "新目录应存在");
        assert!(
            out.dir.to_string_lossy().contains("图像处理第一讲"),
            "{:?}",
            out.dir
        );

        // 重新读出来:标题与 slug 都应更新
        let reloaded = store.find("hash_a").unwrap().unwrap();
        assert_eq!(reloaded.meta.title, "图像处理第一讲");
        assert!(reloaded.meta.slug.contains("图像处理第一讲"));
        assert_eq!(reloaded.dir, out.dir);
    }

    #[test]
    fn rename_preserves_date_prefix() {
        // ★ 日期前缀是**创建日期**,不是修改日期。
        //   重命名不该让它跳到"今天",否则按时间排序会乱。
        let (_d, store) = fixture();
        let p = store.create_or_open("h", "旧名").unwrap();
        let old_slug = p.meta.slug.clone();
        let date = old_slug.split('_').next().unwrap().to_string();

        let out = store.rename("h", "新名").unwrap();
        let new_slug = out.dir.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            new_slug.starts_with(&format!("{date}_")),
            "日期前缀应保持 {date},实际 {new_slug}"
        );
    }

    #[test]
    fn rename_keeps_project_id_and_contents() {
        let (_d, store) = fixture();
        let p = store.create_or_open("hash_keep", "原名").unwrap();
        // 放点内容进去,确认移动后还在
        std::fs::write(p.dir.join(TRANSCRIPT_MD), "转写内容").unwrap();

        let out = store.rename("hash_keep", "改名后").unwrap();

        assert_eq!(store.find("hash_keep").unwrap().unwrap().meta.id, "hash_keep");
        assert_eq!(
            std::fs::read_to_string(out.dir.join(TRANSCRIPT_MD)).unwrap(),
            "转写内容",
            "★ 改名不能丢内容"
        );
        assert!(out.dir.join(AUDIO_DIR).is_dir(), "audio 子目录也要跟着走");
    }

    #[test]
    fn rename_to_same_name_only_updates_title() {
        let (_d, store) = fixture();
        let p = store.create_or_open("h", "同一个名字").unwrap();
        let dir = p.dir.clone();

        let out = store.rename("h", "同一个名字").unwrap();
        assert!(!out.slug_changed, "名字没变就不该动目录");
        assert_eq!(out.dir, dir);
        assert_eq!(store.find("h").unwrap().unwrap().meta.title, "同一个名字");
    }

    #[test]
    fn rename_handles_illegal_chars_in_title() {
        let (_d, store) = fixture();
        store.create_or_open("h", "原名").unwrap();
        // Windows 目录名不允许这些字符
        let out = store.rename("h", "a/b:c*d?e").unwrap();
        let name = out.dir.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.contains(['/', ':', '*', '?']), "{name}");
    }

    #[test]
    fn rename_to_existing_name_gets_suffix() {
        let (_d, store) = fixture();
        let a = store.create_or_open("h1", "重名测试").unwrap();
        let b = store.create_or_open("h2", "另一个").unwrap();
        // 让两个工程同一天,这样 slug 的日期前缀一致
        let date = a.meta.slug.split('_').next().unwrap().to_string();

        let out = store.rename("h2", "重名测试").unwrap();
        let name = out.dir.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with(&date), "{name}");
        assert!(name.contains("重名测试"));
        assert_ne!(out.dir, a.dir, "★ 不能覆盖已有工程");
        assert!(a.dir.is_dir(), "被撞名的工程必须完好");
        assert!(b.dir.exists() == false, "原目录应已移走");
    }

    #[test]
    fn rename_rejects_empty_title() {
        let (_d, store) = fixture();
        store.create_or_open("h", "原名").unwrap();
        assert!(store.rename("h", "").is_err());
        assert!(store.rename("h", "   ").is_err());
    }

    #[test]
    fn rename_rejects_unknown_project() {
        let (_d, store) = fixture();
        let e = store.rename("nope", "新名").unwrap_err();
        assert!(e.to_string().contains("找不到"), "{e}");
    }

    #[test]
    fn rename_trims_whitespace() {
        let (_d, store) = fixture();
        store.create_or_open("h", "原名").unwrap();
        let out = store.rename("h", "  前后有空格  ").unwrap();
        assert_eq!(store.find("h").unwrap().unwrap().meta.title, "前后有空格");
        let name = out.dir.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.ends_with(' '), "目录名不该以空格结尾:{name}");
    }

    #[test]
    fn rename_supports_id_prefix() {
        let (_d, store) = fixture();
        store.create_or_open("abcdef123456", "原名").unwrap();
        let out = store.rename("abcdef", "用前缀改名").unwrap();
        assert!(out.dir.to_string_lossy().contains("用前缀改名"));
    }

    #[test]
    fn slug_date_prefix_extracts_only_valid_dates() {
        assert_eq!(
            slug_date_prefix("2026-09-11_高等数学"),
            Some("2026-09-11".into())
        );
        assert_eq!(slug_date_prefix("没日期_标题"), None);
        assert_eq!(slug_date_prefix("2026-09-1_标题"), None, "半个日期不算");
        assert_eq!(slug_date_prefix("no-underscore"), None);
        assert_eq!(slug_date_prefix("2026/09/11_标题"), None);
    }

    #[test]
    fn rename_reports_old_slug_for_sync_warning() {
        // 重命名会改变云端路径,调用方需要知道旧名字才能提醒用户
        let (_d, store) = fixture();
        let p = store.create_or_open("h", "旧名字").unwrap();
        let before = p.meta.slug.clone();
        let out = store.rename("h", "新名字").unwrap();
        assert_eq!(out.old_slug, before);
        assert_ne!(out.old_slug, store.find("h").unwrap().unwrap().meta.slug);
    }
}
