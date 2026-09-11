//! 每文件的本地同步状态。
//!
//! # 为什么需要单独一份本地状态
//!
//! 远端的 manifest 记录的是"**远端有什么**",它回答不了用户最关心的问题:
//!
//! - 这个文件**传上去了吗**?
//! - 还是我把它排除同步了?
//! - 还是我本地删了、云端那份还留着?
//!
//! 要回答这些,必须记"**这台设备上,上一次同步时发生了什么**"。
//! 这就是本模块的职责。
//!
//! # 与 manifest 的分工
//!
//! | | manifest | SyncState |
//! |---|---|---|
//! | 回答 | 远端有什么 | 本机同步到哪一步了 |
//! | 存哪 | 本地 + 远端(会同步) | **只在本地** |
//! | 冲突 | 按 `synced_at` 合并 | 不需要(各设备独立) |
//!
//! `last_synced_at` 是关键:它让"**从未同步过**"和"**同步过但现在不在范围内**"
//! 能区分开。没有它,用户勾掉一个文件后就无从判断它到底传没传过。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 一个文件的显示状态(给 UI 用)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyncStatus {
    /// 本机与远端一致
    Synced,
    /// 本地有更新,等着上传
    PendingUpload,
    /// 远端有更新,等着下载
    PendingDownload,
    /// 本地已删除,按策略会从云端删掉
    PendingDelete,
    /// **被排除在同步范围之外** —— 用户主动不同步它
    Excluded,
    /// 从未同步过(可能是还没轮到,也可能被排除)
    NeverSynced,
    /// 本地已删除,但策略不让删云端(云端那份保留着)
    DeletedLocalKeptRemote,
    /// 需要人工处理(冲突/失败)
    Conflict,
}

impl SyncStatus {
    /// 中文标签。
    pub fn label(&self) -> &'static str {
        match self {
            SyncStatus::Synced => "已同步",
            SyncStatus::PendingUpload => "待上传",
            SyncStatus::PendingDownload => "待下载",
            SyncStatus::PendingDelete => "待删除云端",
            SyncStatus::Excluded => "未同步",
            SyncStatus::NeverSynced => "从未同步",
            SyncStatus::DeletedLocalKeptRemote => "本地已删(云端保留)",
            SyncStatus::Conflict => "需处理",
        }
    }

    /// 是否是一个"需要注意"的状态 —— 用于在界面上打提醒标记。
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            SyncStatus::PendingUpload
                | SyncStatus::PendingDownload
                | SyncStatus::PendingDelete
                | SyncStatus::Excluded
                | SyncStatus::Conflict
        )
    }

    /// 是否是"稳定/正常"状态。
    pub fn is_settled(&self) -> bool {
        matches!(self, SyncStatus::Synced)
    }

    /// 在界面上的符号。
    pub fn glyph(&self) -> &'static str {
        match self {
            SyncStatus::Synced => "✓",
            SyncStatus::PendingUpload => "↑",
            SyncStatus::PendingDownload => "↓",
            SyncStatus::PendingDelete => "✗",
            SyncStatus::Excluded => "—",
            SyncStatus::NeverSynced => "·",
            SyncStatus::DeletedLocalKeptRemote => "◌",
            SyncStatus::Conflict => "!",
        }
    }

    /// 汇总多个子项的状态(给文件夹节点用)。
    ///
    /// 规则是"**取最需要注意的那个**",因为文件夹的状态要能提醒用户
    /// 里面有事没处理完 —— 全同步的文件夹里藏着一个没上传的文件,
    /// 那个文件夹就该显示"待上传"而不是"已同步"。
    ///
    /// 优先级:需处理 > 待删除 > 待下载 > 待上传 > 未同步 >
    ///         本地已删(云端保留)> 从未同步 > 已同步
    pub fn aggregate(children: &[SyncStatus]) -> Option<SyncStatus> {
        if children.is_empty() {
            return None;
        }
        let rank = |s: SyncStatus| match s {
            SyncStatus::Conflict => 0,
            SyncStatus::PendingDelete => 1,
            SyncStatus::PendingDownload => 2,
            SyncStatus::PendingUpload => 3,
            SyncStatus::Excluded => 4,
            SyncStatus::DeletedLocalKeptRemote => 5,
            SyncStatus::NeverSynced => 6,
            SyncStatus::Synced => 7,
        };
        children.iter().copied().min_by_key(|s| rank(*s))
    }

    /// 是否"部分参与同步" —— 文件夹里有被排除的子项时用。
    ///
    /// 界面上用它显示成半选的复选框。
    pub fn is_partial(in_scope_flags: &[bool]) -> bool {
        let any_in = in_scope_flags.iter().any(|b| *b);
        let any_out = in_scope_flags.iter().any(|b| !*b);
        any_in && any_out
    }
}

/// 单个文件的同步记录(**仅本机**)。
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FileSyncRecord {
    /// 最后成功同步的毫秒时间戳。None = 从未同步过。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced_at: Option<i64>,
    /// 同步时的远端 ETag
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// 同步时的本地大小(用于判断本地是否变过)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// 本机同步状态表。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncState {
    pub version: u32,
    pub files: BTreeMap<String, FileSyncRecord>,
}

impl Default for SyncState {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncState {
    pub const VERSION: u32 = 1;

    pub fn new() -> Self {
        Self {
            version: Self::VERSION,
            files: BTreeMap::new(),
        }
    }

    pub fn path_in(root: &Path) -> PathBuf {
        root.join("sync-state.json")
    }

    /// 读取。文件不存在或损坏时返回空表。
    ///
    /// **损坏不该是致命错误** —— 这份状态只影响界面显示,
    /// 丢了最多是"状态显示成从未同步",重新同步一次就补回来了。
    pub fn load(path: &Path) -> Self {
        let Ok(raw) = std::fs::read(path) else {
            // 文件不存在是正常情况(还没同步过),不打日志刷屏
            return Self::new();
        };
        // 容忍 UTF-8 BOM(记事本保存会加)
        let bytes = crate::sync::manifest::strip_utf8_bom(&raw);
        match serde_json::from_slice::<SyncState>(bytes) {
            Ok(s) => {
                tracing::debug!(
                    "sync-state 载入 {} 条记录({})",
                    s.files.len(),
                    path.display()
                );
                s
            }
            Err(e) => {
                // 这条一定要响 —— 静默丢掉状态会让"未同步"全部显示成"从未同步",
                // 而用户看不出哪里不对
                tracing::warn!(
                    "sync-state 解析失败({e}),按空表处理 —— 界面状态会不准。文件:{}",
                    path.display()
                );
                Self::new()
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        // 不带 BOM 写入
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn get(&self, rel: &str) -> Option<&FileSyncRecord> {
        self.files.get(rel)
    }

    /// 记录一次成功同步。
    pub fn mark_synced(&mut self, rel: &str, etag: Option<String>, size: Option<u64>) {
        self.files.insert(
            rel.to_string(),
            FileSyncRecord {
                last_synced_at: Some(crate::store::db::now_ms()),
                etag,
                size,
            },
        );
    }

    /// 记录本地删除(保留历史,便于判断"曾经同步过")。
    ///
    /// 不删记录,只把 `size` 清掉 —— `last_synced_at` 还要留着回答
    /// "这个文件到底传过没有"。
    pub fn mark_local_deleted(&mut self, rel: &str) {
        if let Some(r) = self.files.get_mut(rel) {
            r.size = None;
        }
    }

    /// 计算某个文件的显示状态。
    ///
    /// 参数都由调用方提供,**本函数不碰文件系统** —— 它拿到的 `rel` 是
    /// 相对 store 根的路径,直接当绝对路径用会失败(工作目录不一定是 store 根)。
    ///
    /// - `in_scope` 来自选择配置:`false` 表示被排除
    /// - `local_size` 本地文件大小;`None` 表示本地不存在
    /// - `remote_exists` 远端是否存在(未知时传 `None`,会退化为保守判断)
    /// - `deletion_allowed` 删除策略是否允许删云端
    pub fn status_of(
        &self,
        rel: &str,
        in_scope: bool,
        local_size: Option<u64>,
        remote_exists: Option<bool>,
        deletion_allowed: bool,
    ) -> SyncStatus {
        let rec = self.files.get(rel);
        let local_exists = local_size.is_some();

        // 1. 被排除的优先级最高 —— 用户明确说了不同步它
        if !in_scope {
            return if local_exists {
                SyncStatus::Excluded
            } else {
                // 本地没了且不再同步 —— 云端那份会怎样取决于删除策略,
                // 但既然不在范围内,我们不会去动它
                SyncStatus::DeletedLocalKeptRemote
            };
        }

        // 2. 本地不存在
        if !local_exists {
            return match rec.and_then(|r| r.last_synced_at) {
                None => SyncStatus::NeverSynced,
                Some(_) => {
                    if deletion_allowed {
                        SyncStatus::PendingDelete
                    } else {
                        SyncStatus::DeletedLocalKeptRemote
                    }
                }
            };
        }

        // 3. 本地存在但从未同步过
        let Some(rec) = rec else {
            return SyncStatus::NeverSynced;
        };
        if rec.last_synced_at.is_none() {
            return SyncStatus::NeverSynced;
        }

        // 4. 同步过 —— 比对大小判断本地是否变过
        let changed = match (local_size, rec.size) {
            (Some(a), Some(b)) => a != b,
            // 没记录大小(比如从 manifest 推断出的历史记录)→ 无法判断,按未变处理
            _ => false,
        };

        match remote_exists {
            Some(false) => SyncStatus::PendingUpload,
            Some(true) if changed => SyncStatus::PendingUpload,
            Some(true) => SyncStatus::Synced,
            // 远端状态未知(离线)时,乐观地认为已同步
            None => {
                if changed {
                    SyncStatus::PendingUpload
                } else {
                    SyncStatus::Synced
                }
            }
        }
    }

    /// 汇总统计(给界面上的"还有 N 个待同步"用)。
    pub fn counts(&self) -> SyncCounts {
        let mut c = SyncCounts::default();
        for r in self.files.values() {
            if r.last_synced_at.is_some() {
                c.synced += 1;
            } else {
                c.never += 1;
            }
        }
        c
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// 状态计数汇总。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncCounts {
    pub synced: usize,
    pub never: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(rel: &str, synced: bool, size: Option<u64>) -> SyncState {
        let mut s = SyncState::new();
        s.files.insert(
            rel.to_string(),
            FileSyncRecord {
                last_synced_at: if synced { Some(1000) } else { None },
                etag: Some("\"e\"".into()),
                size,
            },
        );
        s
    }

    // --- 状态判定 ----------------------------------------------------------

    #[test]
    fn excluded_wins_over_everything() {
        // ★ 用户主动排除的文件,不管别的条件如何,都显示"未同步"
        let s = state_with("a.md", true, Some(10));
        let st = s.status_of("a.md", false, Some(10), Some(true), true);
        assert_eq!(st, SyncStatus::Excluded);
        assert!(st.needs_attention());
    }

    #[test]
    fn excluded_and_gone_locally_keeps_remote() {
        // 本地没了 + 不在范围内 → 我们不会去动云端
        let s = state_with("a.md", true, Some(10));
        let st = s.status_of("a.md", false, None, Some(true), true);
        assert_eq!(st, SyncStatus::DeletedLocalKeptRemote);
    }

    #[test]
    fn never_synced_when_no_record() {
        let s = SyncState::new();
        assert_eq!(
            s.status_of("a.md", true, Some(10), Some(false), true),
            SyncStatus::NeverSynced
        );
    }

    #[test]
    fn never_synced_when_record_has_no_timestamp() {
        // 有记录但没成功时间 = 从未成功同步
        let s = state_with("a.md", false, None);
        assert_eq!(
            s.status_of("a.md", true, Some(10), Some(true), true),
            SyncStatus::NeverSynced
        );
    }

    #[test]
    fn pending_upload_when_remote_missing() {
        let s = state_with("a.md", true, Some(10));
        assert_eq!(
            s.status_of("a.md", true, Some(10), Some(false), true),
            SyncStatus::PendingUpload
        );
    }

    #[test]
    fn pending_delete_when_local_gone_and_allowed() {
        let s = state_with("a.md", true, Some(10));
        let st = s.status_of("a.md", true, None, Some(true), true);
        assert_eq!(st, SyncStatus::PendingDelete);
        assert!(st.needs_attention());
    }

    #[test]
    fn deleted_local_kept_remote_when_deletion_disallowed() {
        // ★ 这就是"本地删了音频但云端保住"的显示状态
        let s = state_with("a/audio/x.wav", true, Some(999));
        let st = s.status_of("a/audio/x.wav", true, None, Some(true), false);
        assert_eq!(st, SyncStatus::DeletedLocalKeptRemote);
        assert_eq!(st.label(), "本地已删(云端保留)");
    }

    #[test]
    fn offline_remote_unknown_is_optimistic() {
        // 远端状态未知时不该满屏"待上传"吓唬人
        let s = state_with("a.md", true, Some(10));
        assert_eq!(
            s.status_of("a.md", true, Some(10), None, true),
            SyncStatus::Synced
        );
    }

    // --- 记录维护 ----------------------------------------------------------

    #[test]
    fn mark_synced_sets_timestamp() {
        let mut s = SyncState::new();
        assert!(s.get("a.md").is_none());
        s.mark_synced("a.md", Some("\"e\"".into()), Some(42));
        let r = s.get("a.md").unwrap();
        assert!(r.last_synced_at.is_some());
        assert_eq!(r.size, Some(42));
        assert_eq!(r.etag.as_deref(), Some("\"e\""));
    }

    #[test]
    fn mark_local_deleted_keeps_history() {
        // ★ 关键:删本地不能抹掉"曾经同步过"这个事实,
        //   否则就分不清"删了"和"从没传过"
        let mut s = SyncState::new();
        s.mark_synced("a.md", Some("\"e\"".into()), Some(42));
        s.mark_local_deleted("a.md");
        let r = s.get("a.md").unwrap();
        assert_eq!(r.size, None, "大小应清掉");
        assert!(r.last_synced_at.is_some(), "★ 同步历史必须保留");
    }

    #[test]
    fn mark_local_deleted_on_unknown_file_is_noop() {
        let mut s = SyncState::new();
        s.mark_local_deleted("nope.md");
        assert!(s.get("nope.md").is_none(), "不该凭空造记录");
    }

    // --- 持久化 ------------------------------------------------------------

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = SyncState::path_in(tmp.path());

        let mut s = SyncState::new();
        s.mark_synced("a/b.md", Some("\"e\"".into()), Some(7));
        s.save(&path).unwrap();

        let back = SyncState::load(&path);
        assert_eq!(back.get("a/b.md").unwrap().size, Some(7));
    }

    #[test]
    fn saved_file_has_no_bom() {
        let tmp = tempfile::tempdir().unwrap();
        let path = SyncState::path_in(tmp.path());
        SyncState::new().save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
            "★ 不该写 BOM —— 否则别的解析器会读不出来"
        );
    }

    #[test]
    fn load_tolerates_bom() {
        // 记事本保存过的情况
        let tmp = tempfile::tempdir().unwrap();
        let path = SyncState::path_in(tmp.path());
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice(br#"{"version":1,"files":{"a.md":{"size":5}}}"#);
        std::fs::write(&path, with_bom).unwrap();

        let s = SyncState::load(&path);
        assert_eq!(s.get("a.md").unwrap().size, Some(5));
    }

    #[test]
    fn load_missing_file_gives_empty_state() {
        let tmp = tempfile::tempdir().unwrap();
        let s = SyncState::load(&SyncState::path_in(tmp.path()));
        assert!(s.is_empty());
    }

    #[test]
    fn load_corrupt_file_gives_empty_state_not_panic() {
        // ★ 状态表损坏不该让程序崩 —— 它只影响显示
        let tmp = tempfile::tempdir().unwrap();
        let path = SyncState::path_in(tmp.path());
        std::fs::write(&path, b"{{{ not json").unwrap();
        let s = SyncState::load(&path);
        assert!(s.is_empty());
    }

    // --- 其他 --------------------------------------------------------------

    #[test]
    fn counts_summarize() {
        let mut s = SyncState::new();
        s.mark_synced("a", None, None);
        s.mark_synced("b", None, None);
        s.files.insert("c".into(), FileSyncRecord::default());
        let c = s.counts();
        assert_eq!(c.synced, 2);
        assert_eq!(c.never, 1);
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn status_labels_and_glyphs_are_distinct() {
        let all = [
            SyncStatus::Synced,
            SyncStatus::PendingUpload,
            SyncStatus::PendingDownload,
            SyncStatus::PendingDelete,
            SyncStatus::Excluded,
            SyncStatus::NeverSynced,
            SyncStatus::DeletedLocalKeptRemote,
            SyncStatus::Conflict,
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(|s| s.label()).collect();
        assert_eq!(labels.len(), all.len(), "每种状态的标签应不同");
        let glyphs: std::collections::HashSet<_> = all.iter().map(|s| s.glyph()).collect();
        assert_eq!(glyphs.len(), all.len(), "每种状态的符号应不同");
        assert!(SyncStatus::Synced.is_settled());
        assert!(!SyncStatus::Excluded.is_settled());
    }

    #[test]
    fn json_omits_none_fields() {
        let r = FileSyncRecord::default();
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, "{}", "全 None 时应序列化成空对象");
    }
}
