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

        let mut cand = base.join(&stem);
        let mut n = 2;
        while cand.exists() {
            cand = base.join(format!("{stem}_{n}"));
            n += 1;
            if n > 999 {
                return Err(anyhow!("同名工程过多,无法生成目录名"));
            }
        }
        Ok(cand)
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

    #[test]
    fn date_prefix_shape() {
        let d = date_prefix();
        assert_eq!(d.len(), 10, "{d}");
        assert_eq!(&d[4..5], "-");
        assert_eq!(&d[7..8], "-");
    }
}
