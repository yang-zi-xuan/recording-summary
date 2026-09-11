//! 同步清单(manifest)。见技术方案 §11.6。
//!
//! **manifest 是"事实的追加日志",不是"状态的快照"。** 这个区别决定了实现复杂度:
//!
//! - 每条记录只追加不删(文件存在这个事实不会变)
//! - 两边都有 → 按 `synced_at` 取新的合并
//! - 上传前先 GET 最新 manifest,合并后再 PUT —— 这就是单人场景的锁
//!
//! **★ 为什么它不需要事务保护:** manifest 随时可以从本地文件 + 远端 PROPFIND
//! 重新生成(见 `Syncer::rebuild_manifest`)。坏了就重建,所以不必做校验和、
//! 版本迁移、回滚那一套。这砍掉了同步系统里最容易出 bug 的部分。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// manifest 在远端的固定位置。
pub const MANIFEST_REMOTE_PATH: &str = "manifest.json";

/// 剥掉 UTF-8 BOM(`EF BB BF`)。
///
/// Windows 记事本、部分编辑器、以及 `Out-File -Encoding utf8`(Windows PowerShell 5.1)
/// 都会写这个前缀。JSON 规范不允许它,但现实中很常见,所以解析前统一剥掉。
pub fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &bytes[3..]
    } else {
        bytes
    }
}

/// 当前格式版本。
pub const FORMAT_VERSION: u32 = 1;

/// 单个文件的一条同步记录。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ManifestEntry {
    /// 远端 ETag。None 表示尚未确认(例如刚记录、还没上传成功)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// 毫秒时间戳。冲突时取新的
    pub synced_at: i64,

    /// 哪台设备最后写的这条记录。
    ///
    /// **这是"安全删除"的关键。** 没有它就没法区分两种情况:
    ///
    /// - 这台设备同步过、现在本地没了 → **用户删了它**,可以跟着删云端
    /// - 这台设备从没同步过这个文件 → 只是还没下载,**绝不能删**
    ///
    /// 空值 = 旧版本写的记录,一律按"不是本机"处理(保守:不删)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,

    /// 删除墓碑。Some 表示这条记录代表"已删除"而不是"存在"。
    ///
    /// 为什么要墓碑而不是直接移除记录:直接移除的话,另一台设备再同步时
    /// 会看到"本地有、manifest 里没有",于是判定为"远端没有 → 上传",
    /// 把刚删掉的文件又传回去。墓碑让删除也能传播。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<i64>,
}

impl ManifestEntry {
    /// 是否是墓碑(代表已删除)。
    pub fn is_tombstone(&self) -> bool {
        self.deleted_at.is_some()
    }

    /// 这条记录是否由本机写入。
    pub fn from_device(&self, device: &str) -> bool {
        self.origin.as_deref() == Some(device)
    }
}

/// 同步清单。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// 相对 `store/` 的路径 → 记录
    pub files: BTreeMap<String, ManifestEntry>,
    /// 档案注册表的轻量镜像(只含名字,不含声纹向量)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, String>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self::new()
    }
}

impl Manifest {
    pub fn new() -> Self {
        Self {
            version: FORMAT_VERSION,
            files: BTreeMap::new(),
            profiles: BTreeMap::new(),
        }
    }

    /// 读取;文件不存在或损坏时返回空 manifest。
    ///
    /// **损坏时不报错而是重建** —— 因为 manifest 是可再生的,
    /// 为它中断整个同步不值得。
    pub fn load_or_new(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self::new());
        }
        match std::fs::read(path) {
            Ok(bytes) => match Self::from_bytes(&bytes) {
                Ok(m) => Ok(m),
                Err(e) => {
                    tracing::warn!(
                        "manifest 解析失败({e}),将重建。这是安全的 —— manifest 可从远端扫回来。"
                    );
                    Ok(Self::new())
                }
            },
            Err(e) => {
                tracing::warn!("manifest 读取失败({e}),将重建");
                Ok(Self::new())
            }
        }
    }

    /// 从字节解析。
    ///
    /// **会剥掉 UTF-8 BOM。** 这不是洁癖 —— 实测踩过:
    /// 用 Windows 记事本(或其他会写 BOM 的编辑器)打开并保存 manifest,
    /// 文件开头就多了 `EF BB BF`,而 `serde_json` 不接受前导 BOM,
    /// 于是**整份 manifest 被判定损坏并重建** —— 所有 `origin` 标记丢失,
    /// 删除判定随之失效(变成从不删云端)。
    ///
    /// manifest 本身可以重建所以不算致命,但静默丢功能比报错更糟,
    /// 所以这里显式处理。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let bytes = strip_utf8_bom(bytes);
        let m: Manifest = serde_json::from_slice(bytes).context("解析 manifest JSON 失败")?;
        Ok(m)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let bytes = self.to_bytes()?;
        // 原子写:先临时文件再改名
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 记录一条(只在更"新"时覆盖)。
    pub fn record(&mut self, rel_path: &str, entry: ManifestEntry) {
        match self.files.get(rel_path) {
            Some(existing) if existing.synced_at > entry.synced_at => {}
            _ => {
                self.files.insert(rel_path.to_string(), entry);
            }
        }
    }

    /// 记录一次删除(墓碑)。
    ///
    /// 墓碑与普通记录走同一套 `synced_at` 冲突解决 —— 谁新谁赢。
    /// 这样"删了又上传"和"上传了又删"都能正确收敛到最后一次操作。
    pub fn record_tombstone(&mut self, rel_path: &str, device: &str) {
        self.record(
            rel_path,
            ManifestEntry {
                etag: None,
                size: None,
                synced_at: crate::store::db::now_ms(),
                origin: Some(device.to_string()),
                deleted_at: Some(crate::store::db::now_ms()),
            },
        );
    }

    /// 当前"应该存在"的文件(排除墓碑)。
    pub fn live_files(&self) -> impl Iterator<Item = (&String, &ManifestEntry)> {
        self.files.iter().filter(|(_, e)| !e.is_tombstone())
    }

    /// 全部墓碑。
    pub fn tombstones(&self) -> impl Iterator<Item = (&String, &ManifestEntry)> {
        self.files.iter().filter(|(_, e)| e.is_tombstone())
    }

    /// 取某文件的记录。
    pub fn get(&self, rel_path: &str) -> Option<&ManifestEntry> {
        self.files.get(rel_path)
    }

    /// 合并另一份 manifest(通常是远端的)。
    ///
    /// 追加语义:两边都有的路径取 `synced_at` 更大的那条;
    /// 只有一边有的直接并入。**不删除任何条目。**
    pub fn merge_from(&mut self, other: &Manifest) {
        for (k, v) in &other.files {
            self.record(k, v.clone());
        }
        for (k, v) in &other.profiles {
            self.profiles
                .entry(k.clone())
                .or_insert_with(|| v.clone());
        }
        self.version = self.version.max(other.version);
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// 记录档案名字(不含向量 —— 向量永不出本机)。
    pub fn record_profile(&mut self, profile_id: &str, display_name: &str) {
        self.profiles
            .insert(profile_id.to_string(), display_name.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(etag: &str, ts: i64) -> ManifestEntry {
        ManifestEntry {
            etag: Some(etag.into()),
            size: Some(100),
            synced_at: ts,
            origin: None,
            deleted_at: None,
        }
    }

    #[test]
    fn tombstone_marks_deleted() {
        let mut m = Manifest::new();
        m.record("a.md", entry("\"e\"", 100));
        assert!(!m.get("a.md").unwrap().is_tombstone());
        assert_eq!(m.live_files().count(), 1);

        m.record_tombstone("a.md", "dev-1");
        let e = m.get("a.md").unwrap();
        assert!(e.is_tombstone());
        assert!(e.from_device("dev-1"));
        assert!(!e.from_device("dev-2"));
        assert_eq!(m.live_files().count(), 0, "墓碑不算存活");
        assert_eq!(m.tombstones().count(), 1);
    }

    #[test]
    fn older_tombstone_does_not_beat_newer_upload() {
        // "删了又传" 应收敛到上传
        let mut m = Manifest::new();
        m.record("a.md", entry("\"e\"", 200));
        m.record(
            "a.md",
            ManifestEntry {
                etag: None,
                size: None,
                synced_at: 100, // 更旧
                origin: Some("dev-1".into()),
                deleted_at: Some(100),
            },
        );
        assert!(!m.get("a.md").unwrap().is_tombstone(), "更旧的墓碑不该生效");
    }

    #[test]
    fn newer_tombstone_beats_older_upload() {
        // "传了又删" 应收敛到删除
        let mut m = Manifest::new();
        m.record("a.md", entry("\"e\"", 100));
        m.record_tombstone("a.md", "dev-1");
        assert!(m.get("a.md").unwrap().is_tombstone());
    }

    #[test]
    fn legacy_entry_without_new_fields_still_parses() {
        // 旧版本写的 manifest 没有 origin / deleted_at
        let legacy = r#"{"etag":"\"e\"","size":100,"synced_at":123}"#;
        let e: ManifestEntry = serde_json::from_str(legacy).unwrap();
        assert_eq!(e.synced_at, 123);
        assert!(!e.is_tombstone());
        assert!(
            !e.from_device("anything"),
            "★ 旧记录一律按'不是本机'处理 —— 保守,不删"
        );
    }

    #[test]
    fn new_manifest_is_empty_and_versioned() {
        let m = Manifest::new();
        assert!(m.is_empty());
        assert_eq!(m.version, FORMAT_VERSION);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let m = Manifest::load_or_new(&dir.path().join("nope.json")).unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn load_corrupt_file_rebuilds_instead_of_failing() {
        // ★ 这是刻意的:manifest 可再生,不值得为它中断同步
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("manifest.json");
        std::fs::write(&p, b"{ this is not json").unwrap();
        let m = Manifest::load_or_new(&p).unwrap();
        assert!(m.is_empty(), "损坏时应重建为空而不是报错");
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("manifest.json");
        let mut m = Manifest::new();
        m.record("transcript/ab/x.json", entry("\"e1\"", 100));
        m.record_profile("p_1", "张老师");
        m.save(&p).unwrap();

        let back = Manifest::load_or_new(&p).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back.get("transcript/ab/x.json").unwrap().etag.as_deref(), Some("\"e1\""));
        assert_eq!(back.profiles["p_1"], "张老师");
    }

    #[test]
    fn atomic_save_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("manifest.json");
        Manifest::new().save(&p).unwrap();
        assert!(!p.with_extension("tmp").exists());
    }

    #[test]
    fn record_keeps_newer_entry() {
        let mut m = Manifest::new();
        m.record("a.json", entry("\"old\"", 100));
        m.record("a.json", entry("\"new\"", 200));
        assert_eq!(m.get("a.json").unwrap().etag.as_deref(), Some("\"new\""));

        // 反过来:更旧的不应覆盖更新的
        m.record("a.json", entry("\"stale\"", 50));
        assert_eq!(
            m.get("a.json").unwrap().etag.as_deref(),
            Some("\"new\""),
            "★ 旧记录不能覆盖新记录"
        );
    }

    #[test]
    fn merge_is_append_only_and_takes_newest() {
        let mut local = Manifest::new();
        local.record("a.json", entry("\"local-a\"", 100));
        local.record("b.json", entry("\"local-b\"", 300));

        let mut remote = Manifest::new();
        remote.record("a.json", entry("\"remote-a\"", 200)); // 更新
        remote.record("c.json", entry("\"remote-c\"", 150)); // 本地没有

        local.merge_from(&remote);

        assert_eq!(local.len(), 3, "合并后应包含双方的条目");
        assert_eq!(
            local.get("a.json").unwrap().etag.as_deref(),
            Some("\"remote-a\""),
            "冲突时取 synced_at 更大的"
        );
        assert_eq!(
            local.get("b.json").unwrap().etag.as_deref(),
            Some("\"local-b\""),
            "★ 合并绝不删除已有条目"
        );
        assert!(local.get("c.json").is_some());
    }

    #[test]
    fn merge_from_empty_is_noop() {
        let mut m = Manifest::new();
        m.record("a", entry("\"e\"", 1));
        m.merge_from(&Manifest::new());
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn merge_does_not_lose_newer_local_entry() {
        let mut local = Manifest::new();
        local.record("a.json", entry("\"newer\"", 500));
        let mut remote = Manifest::new();
        remote.record("a.json", entry("\"older\"", 100));
        local.merge_from(&remote);
        assert_eq!(local.get("a.json").unwrap().etag.as_deref(), Some("\"newer\""));
    }

    #[test]
    fn profiles_merge_keeps_existing() {
        let mut local = Manifest::new();
        local.record_profile("p_1", "本地名");
        let mut remote = Manifest::new();
        remote.record_profile("p_1", "远端名");
        remote.record_profile("p_2", "另一个");
        local.merge_from(&remote);

        assert_eq!(local.profiles["p_1"], "本地名", "已存在的本地名字不被覆盖");
        assert_eq!(local.profiles["p_2"], "另一个");
    }

    #[test]
    fn manifest_json_never_contains_vectors() {
        // ★ 声纹向量永不出本机 —— manifest 是会上传的文件,绝不能带上向量
        let mut m = Manifest::new();
        m.record("transcript/ab/x.json", entry("\"e\"", 1));
        m.record_profile("p_1", "张老师");
        let s = String::from_utf8(m.to_bytes().unwrap()).unwrap();
        assert!(!s.contains("centroid"), "{s}");
        assert!(!s.contains("embedding"), "{s}");
        assert!(!s.contains("vector"), "{s}");
    }

    #[test]
    fn entry_size_and_etag_skipped_when_none() {
        let e = ManifestEntry {
            etag: None,
            size: None,
            synced_at: 1,
            origin: None,
            deleted_at: None,
        };
        let s = serde_json::to_string(&e).unwrap();
        // 全部可选字段都该被跳过,只留 synced_at
        assert_eq!(s, r#"{"synced_at":1}"#);
    }

    #[test]
    fn from_bytes_rejects_garbage() {
        assert!(Manifest::from_bytes(b"[]").is_err());
        assert!(Manifest::from_bytes(b"not json").is_err());
    }

    #[test]
    fn from_bytes_accepts_utf8_bom() {
        // ★ 回归测试:Windows 记事本保存 manifest 会加 BOM,
        //   早期版本因此判定整份文件损坏并重建 —— 来源标记全丢,删除功能失效。
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice(br#"{"version":1,"files":{}}"#);
        let m = Manifest::from_bytes(&with_bom).expect("带 BOM 的 manifest 应能解析");
        assert_eq!(m.version, 1);
    }

    #[test]
    fn strip_utf8_bom_only_strips_actual_bom() {
        assert_eq!(strip_utf8_bom(b"\xEF\xBB\xBFabc"), b"abc");
        assert_eq!(strip_utf8_bom(b"abc"), b"abc");
        // 只有前两个字节不算 BOM
        assert_eq!(strip_utf8_bom(b"\xEF\xBBabc"), b"\xEF\xBBabc");
        assert_eq!(strip_utf8_bom(b""), b"");
    }

    #[test]
    fn bom_does_not_break_deletion_tracking() {
        // 端到端意图:带 BOM 的 manifest 解析后,origin 依然可用
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice(
            br#"{"version":1,"files":{"a.md":{"etag":"\"e\"","synced_at":9,"origin":"dev-1"}}}"#,
        );
        let m = Manifest::from_bytes(&with_bom).unwrap();
        let e = m.get("a.md").expect("条目应被保留");
        assert!(e.from_device("dev-1"), "★ origin 应可用,否则不会删云端");
    }

    #[test]
    fn version_merge_takes_max() {
        let mut local = Manifest::new();
        local.version = 1;
        let mut remote = Manifest::new();
        remote.version = 5;
        local.merge_from(&remote);
        assert_eq!(local.version, 5);
    }
}
