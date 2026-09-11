//! 文件存储 —— **跨设备同步的真相源**。
//!
//! 见技术方案 §5.1 / §5.3。核心设计:
//!
//! - 分享的是**文件**,不是数据库。数据库可以随时删掉从这些文件重建。
//! - 路径用**内容哈希**做分段目录:去重白送、不需要列目录、避开中文文件名编码问题。
//! - **说话人标签单独拆一个文件** —— 改一个名字如果重写整个转写 JSON(500KB),
//!   同步时要重传整个文件;拆开后只重写几百字节。这是"改名秒级生效且同步代价极低"的前提。

use crate::types::{hash_shard, SpeakerLabels, Summary, Transcript};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 声纹档案的全局注册表(只含 ID 与名字,**不含向量**)。
///
/// 声纹向量永不出本机 —— 它只在有音频的机器上才有用,
/// 独立同步是纯负收益。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProfileRegistry {
    pub version: u32,
    pub profiles: BTreeMap<String, ProfileEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProfileEntry {
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub updated_at: i64,
}

impl ProfileRegistry {
    pub fn new() -> Self {
        Self {
            version: 1,
            profiles: BTreeMap::new(),
        }
    }
}

/// 某会话的标签文件。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LabelsFile {
    pub session_id: String,
    pub labels_version: u32,
    pub labels: BTreeMap<String, LabelEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LabelEntry {
    pub display_name: String,
    pub color: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
}

/// 文件存储根目录。
#[derive(Clone, Debug)]
pub struct FileStore {
    root: PathBuf,
}

impl FileStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for sub in ["transcript", "summary", "speaker_labels", "audio"] {
            let p = self.root.join(sub);
            std::fs::create_dir_all(&p)
                .with_context(|| format!("创建目录失败: {}", p.display()))?;
        }
        Ok(())
    }

    fn sharded(&self, kind: &str, hash: &str, ext: &str) -> PathBuf {
        self.root
            .join(kind)
            .join(hash_shard(hash))
            .join(format!("{hash}.{ext}"))
    }

    /// 转写文件路径。路径就是内容哈希 → 两台设备录到同一节课只存一份。
    pub fn transcript_path(&self, hash: &str) -> PathBuf {
        self.sharded("transcript", hash, "json")
    }

    pub fn summary_path(&self, hash: &str) -> PathBuf {
        self.sharded("summary", hash, "md")
    }

    pub fn labels_path(&self, hash: &str) -> PathBuf {
        self.sharded("speaker_labels", hash, "json")
    }

    pub fn registry_path(&self) -> PathBuf {
        self.root.join("profiles.json")
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    // -----------------------------------------------------------------------
    // 转写
    // -----------------------------------------------------------------------

    pub fn write_transcript(&self, t: &Transcript, session_id: &str) -> Result<PathBuf> {
        let p = self.transcript_path(session_id);
        write_json(&p, t)?;
        Ok(p)
    }

    pub fn read_transcript(&self, session_id: &str) -> Result<Option<Transcript>> {
        read_json(&self.transcript_path(session_id))
    }

    pub fn has_transcript(&self, session_id: &str) -> bool {
        self.transcript_path(session_id).is_file()
    }

    // -----------------------------------------------------------------------
    // 总结
    // -----------------------------------------------------------------------

    pub fn write_summary(&self, s: &Summary, session_id: &str) -> Result<PathBuf> {
        let p = self.summary_path(session_id);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, &s.content_md)
            .with_context(|| format!("写入纪要失败: {}", p.display()))?;
        // 元数据另存,便于判断是否因改名而过时
        let meta = p.with_extension("meta.json");
        write_json(&meta, s)?;
        Ok(p)
    }

    pub fn read_summary_meta(&self, session_id: &str) -> Result<Option<Summary>> {
        read_json(&self.summary_path(session_id).with_extension("meta.json"))
    }

    pub fn read_summary_markdown(&self, session_id: &str) -> Result<Option<String>> {
        let p = self.summary_path(session_id);
        if p.is_file() {
            Ok(Some(std::fs::read_to_string(&p)?))
        } else {
            Ok(None)
        }
    }

    // -----------------------------------------------------------------------
    // 说话人标签(独立拆分,改名只重写这个)
    // -----------------------------------------------------------------------

    pub fn write_labels(&self, labels: &SpeakerLabels) -> Result<PathBuf> {
        let file = LabelsFile {
            session_id: labels.session_id.clone(),
            labels_version: labels.labels_version,
            labels: labels
                .labels
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string(),
                        LabelEntry {
                            display_name: v.display_name.clone(),
                            color: v.color.clone(),
                            profile_id: v.profile_id.clone(),
                        },
                    )
                })
                .collect(),
        };
        let p = self.labels_path(&labels.session_id);
        write_json(&p, &file)?;
        Ok(p)
    }

    pub fn read_labels(&self, session_id: &str) -> Result<Option<SpeakerLabels>> {
        let p = self.labels_path(session_id);
        let file: Option<LabelsFile> = read_json(&p)?;
        Ok(file.map(|f| {
            let mut labels = SpeakerLabels::new(f.session_id);
            labels.labels_version = f.labels_version;
            for (k, v) in f.labels {
                if let Ok(id) = k.parse::<u32>() {
                    labels.labels.insert(
                        id,
                        crate::types::SpeakerLabel {
                            display_name: v.display_name,
                            color: v.color,
                            profile_id: v.profile_id,
                        },
                    );
                }
            }
            labels
        }))
    }

    // -----------------------------------------------------------------------
    // 档案注册表
    // -----------------------------------------------------------------------

    pub fn write_registry(&self, reg: &ProfileRegistry) -> Result<PathBuf> {
        let p = self.registry_path();
        write_json(&p, reg)?;
        Ok(p)
    }

    pub fn read_registry(&self) -> Result<ProfileRegistry> {
        Ok(read_json(&self.registry_path())?.unwrap_or_else(ProfileRegistry::new))
    }

    // -----------------------------------------------------------------------
    // 待同步文本清单(供 WebDAV 增量同步,见技术方案 §11)
    // -----------------------------------------------------------------------

    /// 列出所有同步文本文件及其相对路径。manifest 用它们比对 ETag。
    pub fn list_sync_files(&self) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for sub in ["transcript", "summary", "speaker_labels"] {
            let base = self.root.join(sub);
            if !base.is_dir() {
                continue;
            }
            for entry in walkdir::WalkDir::new(&base)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if entry.file_type().is_file() {
                    out.push(entry.path().to_path_buf());
                }
            }
        }
        for f in ["profiles.json", "manifest.json"] {
            let p = self.root.join(f);
            if p.is_file() {
                out.push(p);
            }
        }
        out.sort();
        Ok(out)
    }

    /// 转成相对 root 的斜杠路径(WebDAV 用)。
    pub fn relative_sync_path(&self, abs: &Path) -> Option<String> {
        abs.strip_prefix(&self.root).ok().map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join("/")
        })
    }

    /// 列出全部工程目录里的文件(供 WebDAV 同步,见技术方案 §17.5)。
    ///
    /// `include_audio` 决定是否把 `projects/*/audio/` 下的录音也算进来。
    /// **默认不含** —— 音频体积大,更适合交给网盘的桌面客户端(有分块与续传);
    /// 但工程要"整包带走"时就需要它。
    pub fn list_project_files(&self, include_audio: bool) -> Result<Vec<PathBuf>> {
        let base = self.root.join(crate::project::PROJECTS_DIR);
        if !base.is_dir() {
            return Ok(vec![]);
        }
        let audio_seg = crate::project::AUDIO_DIR;
        let mut out = Vec::new();
        for entry in walkdir::WalkDir::new(&base)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !include_audio {
                let under_audio = path
                    .strip_prefix(&base)
                    .map(|rel| rel.components().any(|c| c.as_os_str() == audio_seg))
                    .unwrap_or(false);
                if under_audio {
                    continue;
                }
            }
            out.push(path.to_path_buf());
        }
        out.sort();
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// JSON 读写辅助
// ---------------------------------------------------------------------------

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(value)?;
    // 先写临时文件再改名,避免半截文件被另一台设备读到
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text.as_bytes())
        .with_context(|| format!("写入失败: {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("改名失败: {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取失败: {}", path.display()))?;
    Ok(Some(serde_json::from_str(&text).with_context(|| {
        format!("解析 JSON 失败: {}", path.display())
    })?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Backend, Scene, Segment, TokenUsage};

    fn store() -> (tempfile::TempDir, FileStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = FileStore::new(dir.path());
        s.ensure_dirs().unwrap();
        (dir, s)
    }

    fn sample_transcript() -> Transcript {
        Transcript {
            engine: "whisper.cpp".into(),
            model: "large-v3-turbo".into(),
            backend: Backend::Cuda,
            backend_diarize: Some(Backend::Cpu),
            language: Some("zh".into()),
            segments: vec![Segment::new(0, 1000, "今天讲神经网络")],
            raw_text: "今天讲神经网络".into(),
            duration_ms: 1000,
            diarize: None,
        }
    }

    #[test]
    fn transcript_roundtrip() {
        let (_d, s) = store();
        let t = sample_transcript();
        let p = s.write_transcript(&t, "ab3f9c11").unwrap();
        assert!(p.to_string_lossy().contains("transcript"));
        // 分段目录应是哈希前两位
        assert!(p.to_string_lossy().contains("ab"), "路径应含分段目录: {p:?}");
        let back = s.read_transcript("ab3f9c11").unwrap().unwrap();
        assert_eq!(back.segments.len(), 1);
        assert_eq!(back.backend, Backend::Cuda);
        assert_eq!(back.language.as_deref(), Some("zh"));
    }

    #[test]
    fn missing_files_return_none_not_error() {
        let (_d, s) = store();
        assert!(s.read_transcript("nope").unwrap().is_none());
        assert!(s.read_labels("nope").unwrap().is_none());
        assert!(s.read_summary_markdown("nope").unwrap().is_none());
        assert!(s.read_summary_meta("nope").unwrap().is_none());
        assert!(!s.has_transcript("nope"));
    }

    #[test]
    fn sharded_paths_are_stable_and_distinct() {
        let (_d, s) = store();
        assert_eq!(s.transcript_path("abc123"), s.transcript_path("abc123"));
        assert_ne!(s.transcript_path("abc123"), s.summary_path("abc123"));
        assert_ne!(s.transcript_path("abc123"), s.labels_path("abc123"));
    }

    #[test]
    fn summary_writes_markdown_and_meta_separately() {
        let (_d, s) = store();
        let sum = Summary {
            content_md: "# 纪要\n\n- 知识点".into(),
            scene: Scene::Lecture,
            model: "deepseek-chat".into(),
            usage: TokenUsage {
                input: 1500,
                output: 300,
                cached_input: 1200,
            },
            labels_version: 2,
            used_map_reduce: true,
        };
        s.write_summary(&sum, "h1").unwrap();
        let md = s.read_summary_markdown("h1").unwrap().unwrap();
        assert!(md.contains("知识点"));
        let meta = s.read_summary_meta("h1").unwrap().unwrap();
        assert_eq!(meta.labels_version, 2);
        assert!(meta.used_map_reduce);
        assert_eq!(meta.usage.cached_input, 1200);
    }

    #[test]
    fn labels_roundtrip_preserves_version_and_names() {
        let (_d, s) = store();
        let mut labels = SpeakerLabels::new("h1");
        labels.ensure([0, 1]);
        labels.rename(0, "张老师");
        s.write_labels(&labels).unwrap();

        let back = s.read_labels("h1").unwrap().unwrap();
        assert_eq!(back.labels_version, 1);
        assert_eq!(back.display_name(0), "张老师");
        assert_eq!(back.display_name(1), "说话人 2");
    }

    #[test]
    fn rename_only_touches_small_labels_file() {
        let (_d, s) = store();
        // 先写一个"大"转写
        let mut t = sample_transcript();
        for i in 0..500 {
            t.segments.push(Segment::new(i * 1000, i * 1000 + 900, "填充内容"));
        }
        s.write_transcript(&t, "h1").unwrap();
        let t_size = std::fs::metadata(s.transcript_path("h1")).unwrap().len();

        let mut labels = SpeakerLabels::new("h1");
        labels.ensure([0]);
        labels.rename(0, "张老师");
        s.write_labels(&labels).unwrap();
        let l_size = std::fs::metadata(s.labels_path("h1")).unwrap().len();

        assert!(
            l_size * 10 < t_size,
            "★ 标签文件应远小于转写({l_size} vs {t_size})—— 改名不能重写大文件"
        );
    }

    #[test]
    fn registry_roundtrip_and_default() {
        let (_d, s) = store();
        let reg = s.read_registry().unwrap();
        assert!(reg.profiles.is_empty());

        let mut reg = ProfileRegistry::new();
        reg.profiles.insert(
            "p_1".into(),
            ProfileEntry {
                display_name: "张老师".into(),
                note: Some("3班".into()),
                updated_at: 123,
            },
        );
        s.write_registry(&reg).unwrap();
        let back = s.read_registry().unwrap();
        assert_eq!(back.profiles["p_1"].display_name, "张老师");
    }

    #[test]
    fn atomic_write_leaves_no_tmp_file() {
        let (_d, s) = store();
        s.write_transcript(&sample_transcript(), "hh").unwrap();
        let tmp = s.transcript_path("hh").with_extension("tmp");
        assert!(!tmp.exists(), "★ 临时文件必须被改名掉,不能留在目录里");
    }

    #[test]
    fn overwrite_is_safe() {
        let (_d, s) = store();
        let mut t = sample_transcript();
        s.write_transcript(&t, "h2").unwrap();
        t.raw_text = "第二版".into();
        s.write_transcript(&t, "h2").unwrap();
        assert_eq!(s.read_transcript("h2").unwrap().unwrap().raw_text, "第二版");
    }

    #[test]
    fn list_sync_files_and_relative_paths() {        let (_d, s) = store();
        s.write_transcript(&sample_transcript(), "aa11").unwrap();
        let mut labels = SpeakerLabels::new("aa11");
        labels.ensure([0]);
        s.write_labels(&labels).unwrap();
        s.write_registry(&ProfileRegistry::new()).unwrap();

        let files = s.list_sync_files().unwrap();
        assert!(files.len() >= 3, "应列出转写+标签+注册表: {files:?}");

        let rels: Vec<String> = files
            .iter()
            .filter_map(|f| s.relative_sync_path(f))
            .collect();
        assert!(rels.iter().any(|r| r.starts_with("transcript/aa/")));
        assert!(rels.iter().any(|r| r == "profiles.json"));
        // 路径分隔符必须是正斜杠(WebDAV 用)
        assert!(rels.iter().all(|r| !r.contains('\\')));
    }

    #[test]
    fn list_project_files_excludes_audio_by_default() {
        // ★ 音频体积大,默认不纳入 WebDAV 同步
        let (_d, s) = store();
        let proj = s.root().join(crate::project::PROJECTS_DIR).join("2026-01-01_测试");
        std::fs::create_dir_all(proj.join(crate::project::AUDIO_DIR)).unwrap();
        std::fs::write(proj.join("transcript.md"), "x").unwrap();
        std::fs::write(proj.join("summary-brief.md"), "x").unwrap();
        std::fs::write(proj.join("mindmap.mmd"), "mindmap").unwrap();
        std::fs::write(proj.join("project.json"), "{}").unwrap();
        std::fs::write(
            proj.join(crate::project::AUDIO_DIR).join("recording.wav"),
            "big",
        )
        .unwrap();

        let without = s.list_project_files(false).unwrap();
        let rels: Vec<String> = without
            .iter()
            .filter_map(|f| s.relative_sync_path(f))
            .collect();
        assert_eq!(without.len(), 4, "应只列出 4 个文本产物: {rels:?}");
        assert!(
            rels.iter().all(|r| !r.contains("/audio/")),
            "默认不该含音频: {rels:?}"
        );
        assert!(rels.iter().any(|r| r.ends_with("transcript.md")));
        assert!(rels.iter().any(|r| r.ends_with("mindmap.mmd")));

        // 打开开关后应含音频
        let with = s.list_project_files(true).unwrap();
        assert_eq!(with.len(), 5, "打开后应多出录音");
        let rels2: Vec<String> = with
            .iter()
            .filter_map(|f| s.relative_sync_path(f))
            .collect();
        assert!(
            rels2.iter().any(|r| r.contains("/audio/recording.wav")),
            "{rels2:?}"
        );
    }

    #[test]
    fn list_project_files_on_empty_store() {
        let (_d, s) = store();
        assert!(s.list_project_files(false).unwrap().is_empty());
        assert!(s.list_project_files(true).unwrap().is_empty());
    }
}
