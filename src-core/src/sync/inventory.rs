//! 同步清单树 —— 给界面用的"哪些文件同步了/没同步"。
//!
//! # 结构
//!
//! 三层,镜像磁盘上的真实层级:
//!
//! ```text
//! 工程 2026-09-11_高数        [✓ 已同步]        ← 可整体勾选
//!   转写                      [✓ 已同步]        ← 按类型分组,可勾选
//!     transcript.md           [✓ 已同步]
//!     transcript.json         [✓ 已同步]
//!     transcript.srt          [✓ 已同步]
//!   录音                      [— 未同步]        ← 组级勾选
//!     recording.wav           [— 未同步]
//! ```
//!
//! **为什么要分组而不是把文件全铺开:** 一个工程有近十个文件,
//! 十个工程就是上百行。按"转写/总结/导图/录音"分组后,常用操作
//! (比如"这个工程的录音别同步")变成一次点击。
//!
//! # 状态从哪来
//!
//! 叶子节点的状态由 [`crate::sync::state::SyncState::status_of`] 算出;
//! 文件夹/分组节点的状态是子节点的**聚合**(取最需要注意的那个),
//! 这样"全同步的文件夹里藏着一个没上传的文件"也能被看见。

use crate::sync::selection::SyncSelection;
use crate::sync::state::{SyncState, SyncStatus};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 清单树的一个节点。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InventoryNode {
    /// 相对 store 根的路径,正斜杠分隔。空字符串 = 虚拟根节点。
    pub rel_path: String,
    /// 显示名(通常是最后一段路径)
    pub name: String,
    /// 是文件夹/分组吗
    pub is_dir: bool,
    /// 参与同步吗
    pub in_scope: bool,
    /// 显示状态
    pub status: SyncStatus,
    /// 这条路径本身是否被用户显式排除(而不是被父节点覆盖)
    pub explicitly_excluded: bool,
    /// 文件大小(仅文件;**本地已删除的文件没有大小**)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// 是否为音频(界面可以特殊标注)
    #[serde(default)]
    pub is_audio: bool,
    /// 本地文件还在吗。`false` = 只剩云端那份(或连云端都没了)
    #[serde(default = "default_true")]
    pub local_exists: bool,
    /// 子节点
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<InventoryNode>,
}

fn default_true() -> bool {
    true
}

impl InventoryNode {
    /// 子树里的文件数。
    pub fn file_count(&self) -> usize {
        if self.is_dir {
            self.children.iter().map(|c| c.file_count()).sum()
        } else {
            1
        }
    }

    /// 子树里参与同步的文件数。
    pub fn in_scope_count(&self) -> usize {
        if self.is_dir {
            self.children.iter().map(|c| c.in_scope_count()).sum()
        } else if self.in_scope {
            1
        } else {
            0
        }
    }

    /// 递归找某个路径的节点。
    pub fn find(&self, rel: &str) -> Option<&InventoryNode> {
        if self.rel_path == rel {
            return Some(self);
        }
        for c in &self.children {
            if let Some(n) = c.find(rel) {
                return Some(n);
            }
        }
        None
    }
}

/// 已知的文件分组(按工程内的角色)。
///
/// 顺序就是界面上的显示顺序 —— 用户最关心的排前面。
const GROUPS: &[(&str, &str)] = &[
    ("brief", "简略总结"),
    ("detailed", "详细总结"),
    ("mindmap", "思维导图"),
    ("transcript", "转写"),
    ("audio", "录音"),
    ("meta", "元数据"),
];

/// 把一个相对路径归到哪个分组。
fn group_of(rel: &str) -> &'static str {
    let file = rel.rsplit('/').next().unwrap_or(rel);
    if crate::sync::selection::is_audio_path(rel) {
        return "audio";
    }
    match file {
        crate::project::SUMMARY_BRIEF => "brief",
        crate::project::SUMMARY_DETAILED => "detailed",
        crate::project::MINDMAP => "mindmap",
        "mindmap-outline.md" => "mindmap",
        crate::project::TRANSCRIPT_MD
        | crate::project::TRANSCRIPT_JSON
        | crate::project::TRANSCRIPT_SRT => "transcript",
        _ => "meta",
    }
}

/// 构建清单树。
///
/// `remote_known` 是"远端有哪些文件"的快照(路径 → 是否存在)。
/// 传空表表示不知道 —— 状态会退化为保守判断(见 `SyncState::status_of`)。
///
/// `manifest` 是"云端该有什么"的权威。传 `None` 时不做过滤。
pub fn build_inventory(
    files: &crate::store::files::FileStore,
    sel: &SyncSelection,
    state: &SyncState,
    remote_known: &BTreeMap<String, bool>,
    manifest: Option<&crate::sync::manifest::Manifest>,
) -> Result<InventoryNode> {
    // 1. 收集所有本地文件(含被排除的 —— 界面上要能看见它们)
    let mut all: Vec<String> = Vec::new();
    for abs in files.list_sync_files()? {
        if let Some(rel) = files.relative_sync_path(&abs) {
            all.push(rel);
        }
    }
    for abs in files.list_project_files(true)? {
        if let Some(rel) = files.relative_sync_path(&abs) {
            all.push(rel);
        }
    }

    // ★ 本地已删除、但曾经同步过的文件也要列出来 ——
    //   "本地删了,云端那份还在"正是用户最需要看见的状态。
    //
    //   **但只列 manifest 还认账的那些。**
    //
    //   sync-state 是"本机传过哪些文件"的历史,它**不跟 manifest 对账**:
    //   条目一旦写入就一直留着。于是手工清理或改名之后,manifest 里
    //   已经没有的记录仍然躺在状态表里,被这里列进清单 —— 用户就在
    //   「同步清单」里看到一个云端早就不存在的工程(标着"待删除云端"),
    //   而「云端管理」里根本找不到它。两个界面各说各话。
    //
    //   manifest 才是"云端该有什么"的权威,所以以它为准过滤。
    let live: Option<std::collections::HashSet<String>> = manifest
        .map(|m| m.live_files().map(|(k, _)| k.clone()).collect());
    for rel in state.files.keys() {
        if let Some(set) = &live {
            if !set.contains(rel) {
                continue;
            }
        }
        all.push(rel.clone());
    }

    all.sort();
    all.dedup();

    let root = files.root();

    // 2. 每个文件算状态
    let mut leaves: Vec<(String, bool, SyncStatus, Option<u64>, bool)> = Vec::new();
    for rel in &all {
        let size = std::fs::metadata(root.join(rel)).map(|m| m.len()).ok();
        let in_scope = sel.wants(rel);
        let remote = remote_known.get(rel).copied();
        let del_ok = sel.should_delete_remote(rel);
        let status = state.status_of(rel, in_scope, size, remote, del_ok);
        leaves.push((
            rel.clone(),
            in_scope,
            status,
            size,
            crate::sync::selection::is_audio_path(rel),
        ));
    }

    // 3. 按「工程 → 分组 → 文件」搭树
    //
    // 不在 projects/ 下的文件(哈希存储、profiles.json)归到"其他"。
    let mut projects: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut loose: Vec<usize> = Vec::new();

    for (i, (rel, ..)) in leaves.iter().enumerate() {
        if let Some(rest) = rel.strip_prefix("projects/") {
            let dir = rest.split('/').next().unwrap_or("").to_string();
            if !dir.is_empty() {
                projects.entry(dir).or_default().push(i);
                continue;
            }
        }
        loose.push(i);
    }

    let mut children = Vec::new();

    for (dir, idxs) in projects {
        let project_path = format!("projects/{dir}");
        children.push(build_project_node(&project_path, &dir, &idxs, &leaves, sel));
    }

    if !loose.is_empty() {
        children.push(build_loose_node(&loose, &leaves, sel));
    }

    // 4. 根节点状态 = 所有子节点聚合
    let child_status: Vec<SyncStatus> = children.iter().map(|c| c.status).collect();
    let root_status = SyncStatus::aggregate(&child_status).unwrap_or(SyncStatus::Synced);

    Ok(InventoryNode {
        rel_path: String::new(),
        name: "全部".into(),
        is_dir: true,
        in_scope: true,
        status: root_status,
        explicitly_excluded: false,
        size: None,
        is_audio: false,
        local_exists: true,
        children,
    })
}

type Leaf = (String, bool, SyncStatus, Option<u64>, bool);

fn build_project_node(
    project_path: &str,
    dir_name: &str,
    idxs: &[usize],
    leaves: &[Leaf],
    sel: &SyncSelection,
) -> InventoryNode {
    // 按分组归类
    let mut groups: BTreeMap<&'static str, Vec<usize>> = BTreeMap::new();
    for &i in idxs {
        groups.entry(group_of(&leaves[i].0)).or_default().push(i);
    }

    let mut group_nodes = Vec::new();
    for (key, label) in GROUPS {
        let Some(gidxs) = groups.get(key) else {
            continue;
        };
        let group_path = format!("{project_path}/{key}");
        let nodes: Vec<InventoryNode> = gidxs
            .iter()
            .map(|&i| leaf_node(&leaves[i], sel))
            .collect();

        let in_scope_flags: Vec<bool> = nodes.iter().map(|n| n.in_scope).collect();
        let statuses: Vec<SyncStatus> = nodes.iter().map(|n| n.status).collect();
        let total: u64 = nodes.iter().filter_map(|n| n.size).sum();

        group_nodes.push(InventoryNode {
            rel_path: group_path,
            name: label.to_string(),
            is_dir: true,
            // 分组自身是否参与:只要有一个子项参与就算(部分参与由状态体现)
            in_scope: in_scope_flags.iter().any(|b| *b),
            status: SyncStatus::aggregate(&statuses).unwrap_or(SyncStatus::Synced),
            explicitly_excluded: sel.user_excludes.iter().any(|e| e == &format!("{project_path}/{key}")),
            size: Some(total),
            is_audio: *key == "audio",
            local_exists: true,
            children: nodes,
        });
    }

    let statuses: Vec<SyncStatus> = group_nodes.iter().map(|n| n.status).collect();
    let flags: Vec<bool> = group_nodes.iter().map(|n| n.in_scope).collect();
    let total: u64 = group_nodes.iter().filter_map(|n| n.size).sum();

    InventoryNode {
        rel_path: project_path.to_string(),
        name: dir_name.to_string(),
        is_dir: true,
        in_scope: flags.iter().any(|b| *b),
        status: SyncStatus::aggregate(&statuses).unwrap_or(SyncStatus::Synced),
        explicitly_excluded: sel.user_excludes.iter().any(|e| e == project_path),
        size: Some(total),
        is_audio: false,
        local_exists: true,
        children: group_nodes,
    }
}

/// 哈希存储等不隶属于工程的文件。
fn build_loose_node(idxs: &[usize], leaves: &[Leaf], sel: &SyncSelection) -> InventoryNode {
    let nodes: Vec<InventoryNode> = idxs.iter().map(|&i| leaf_node(&leaves[i], sel)).collect();
    let statuses: Vec<SyncStatus> = nodes.iter().map(|n| n.status).collect();
    let flags: Vec<bool> = nodes.iter().map(|n| n.in_scope).collect();
    let total: u64 = nodes.iter().filter_map(|n| n.size).sum();

    InventoryNode {
        rel_path: String::new(), // 虚拟节点,不对应真实路径
        name: "其他(哈希存储)".into(),
        is_dir: true,
        in_scope: flags.iter().any(|b| *b),
        status: SyncStatus::aggregate(&statuses).unwrap_or(SyncStatus::Synced),
        explicitly_excluded: false,
        size: Some(total),
        is_audio: false,
        local_exists: true,
        children: nodes,
    }
}

fn leaf_node(leaf: &Leaf, sel: &SyncSelection) -> InventoryNode {
    let (rel, in_scope, status, size, is_audio) = leaf;
    let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
    InventoryNode {
        rel_path: rel.clone(),
        name,
        is_dir: false,
        in_scope: *in_scope,
        status: *status,
        explicitly_excluded: sel.user_excludes.iter().any(|e| e == rel),
        size: *size,
        is_audio: *is_audio,
        local_exists: size.is_some(),
        children: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::files::FileStore;

    fn setup() -> (tempfile::TempDir, FileStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        let p1 = store.join("projects").join("2026-09-11_高数");
        std::fs::create_dir_all(p1.join("audio")).unwrap();
        std::fs::write(p1.join("transcript.md"), "x").unwrap();
        std::fs::write(p1.join("transcript.srt"), "x").unwrap();
        std::fs::write(p1.join("summary-brief.md"), "x").unwrap();
        std::fs::write(p1.join("mindmap.mmd"), "x").unwrap();
        std::fs::write(p1.join("audio").join("rec.wav"), "yy").unwrap();
        std::fs::write(p1.join("project.json"), "{}").unwrap();

        let p2 = store.join("projects").join("2026-09-12_线代");
        std::fs::create_dir_all(&p2).unwrap();
        std::fs::write(p2.join("transcript.md"), "x").unwrap();

        (tmp, FileStore::new(store))
    }

    #[test]
    fn builds_project_group_file_tree() {
        let (_d, files) = setup();
        let sel = SyncSelection::all();
        let inv = build_inventory(&files, &sel, &SyncState::new(), &BTreeMap::new(), None).unwrap();

        assert_eq!(inv.children.len(), 2, "两个工程");
        let p1 = inv.children.iter().find(|c| c.name.contains("高数")).unwrap();

        // 分组应存在且顺序合理
        let names: Vec<&str> = p1.children.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"简略总结"), "{names:?}");
        assert!(names.contains(&"转写"), "{names:?}");
        assert!(names.contains(&"录音"), "{names:?}");
        assert!(names.contains(&"思维导图"), "{names:?}");

        // 转写组里应有 2 个文件
        let tr = p1.children.iter().find(|c| c.name == "转写").unwrap();
        assert_eq!(tr.file_count(), 2);
        assert!(tr.children.iter().any(|c| c.name == "transcript.md"));
        assert!(tr.children.iter().any(|c| c.name == "transcript.srt"));
    }

    #[test]
    fn audio_group_is_marked() {
        let (_d, files) = setup();
        let inv = build_inventory(
            &files,
            &SyncSelection::all(),
            &SyncState::new(),
            &BTreeMap::new(),
            None,
        )
        .unwrap();
        let p1 = inv.children.iter().find(|c| c.name.contains("高数")).unwrap();
        let audio = p1.children.iter().find(|c| c.name == "录音").unwrap();
        assert!(audio.is_audio);
        assert!(audio.children[0].is_audio);
    }

    #[test]
    fn file_count_and_scope_count() {
        let (_d, files) = setup();
        let mut sel = SyncSelection::all();
        let inv = build_inventory(&files, &sel, &SyncState::new(), &BTreeMap::new(), None).unwrap();
        let p1idx = inv
            .children
            .iter()
            .position(|c| c.name.contains("高数"))
            .unwrap();
        let total = inv.children[p1idx].file_count();
        assert_eq!(total, 6, "高数工程有 6 个文件");
        assert_eq!(inv.children[p1idx].in_scope_count(), 6);

        // 排除录音后,参与数应减少
        sel.exclude_path("projects/2026-09-11_高数/audio");
        let inv2 = build_inventory(&files, &sel, &SyncState::new(), &BTreeMap::new(), None).unwrap();
        assert_eq!(inv2.children[p1idx].file_count(), 6, "文件数不变(仍可见)");
        assert_eq!(inv2.children[p1idx].in_scope_count(), 5, "但少一个参与");
    }

    #[test]
    fn excluded_file_shows_excluded_status() {
        let (_d, files) = setup();
        let mut sel = SyncSelection::all();
        sel.exclude_path("projects/2026-09-11_高数/audio");

        let inv = build_inventory(&files, &sel, &SyncState::new(), &BTreeMap::new(), None).unwrap();
        let node = inv
            .find("projects/2026-09-11_高数/audio/rec.wav")
            .expect("应能找到被排除的文件");
        assert_eq!(node.status, SyncStatus::Excluded);
        assert!(!node.in_scope);
    }

    #[test]
    fn group_state_aggregates_from_children() {
        let (_d, files) = setup();
        let mut sel = SyncSelection::all();
        sel.exclude_path("projects/2026-09-11_高数/audio");

        let inv = build_inventory(&files, &sel, &SyncState::new(), &BTreeMap::new(), None).unwrap();
        let audio = inv
            .find("projects/2026-09-11_高数/audio")
            .expect("录音组");
        assert_eq!(audio.status, SyncStatus::Excluded);

        // 父工程的状态也受影响 —— 取最需要注意的子状态
        let proj = inv.find("projects/2026-09-11_高数").unwrap();
        assert_eq!(
            proj.status,
            SyncStatus::Excluded,
            "★ 工程状态应反映'里面有个没同步的'"
        );
    }

    #[test]
    fn synced_files_show_synced() {
        let (_d, files) = setup();
        let mut state = SyncState::new();
        state.mark_synced("projects/2026-09-12_线代/transcript.md", Some("\"e\"".into()), Some(1));

        let mut remote = BTreeMap::new();
        remote.insert("projects/2026-09-12_线代/transcript.md".to_string(), true);

        let inv = build_inventory(&files, &SyncSelection::all(), &state, &remote, None).unwrap();
        let n = inv
            .find("projects/2026-09-12_线代/transcript.md")
            .unwrap();
        assert_eq!(n.status, SyncStatus::Synced);
    }

    #[test]
    fn local_deletion_with_audio_protection_shows_kept_remote() {
        // ★ 录音本地删了但策略保护 → 应显示"本地已删(云端保留)"
        let (_d, files) = setup();
        std::fs::remove_file(
            files
                .root()
                .join("projects/2026-09-11_高数/audio/rec.wav"),
        )
        .unwrap();

        let mut state = SyncState::new();
        state.mark_synced(
            "projects/2026-09-11_高数/audio/rec.wav",
            Some("\"e\"".into()),
            Some(2),
        );

        let mut remote = BTreeMap::new();
        remote.insert("projects/2026-09-11_高数/audio/rec.wav".to_string(), true);

        // 默认 TextOnly 策略:音频不准删
        let inv = build_inventory(&files, &SyncSelection::all(), &state, &remote, None).unwrap();
        let n = inv
            .find("projects/2026-09-11_高数/audio/rec.wav")
            .expect("记录仍在(本地没了但状态表里有)");
        assert_eq!(n.status, SyncStatus::DeletedLocalKeptRemote);
    }

    #[test]
    fn loose_files_go_to_other_bucket() {
        let (_d, files) = setup();
        let hash_dir = files.root().join("transcript").join("ab");
        std::fs::create_dir_all(&hash_dir).unwrap();
        std::fs::write(hash_dir.join("abc.json"), "x").unwrap();

        let inv = build_inventory(
            &files,
            &SyncSelection::all(),
            &SyncState::new(),
            &BTreeMap::new(),
            None,
        )
        .unwrap();
        assert!(
            inv.children.iter().any(|c| c.name.contains("其他")),
            "哈希存储应归到'其他':{:?}",
            inv.children.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn locally_deleted_file_still_listed() {
        // ★ 回归测试:早期版本只遍历本地文件,于是"本地已删、云端保留"
        //   的文件根本不出现在清单里 —— 而那恰恰是用户最该看见的。
        let (_d, files) = setup();
        let rel = "projects/2026-09-11_高数/audio/rec.wav";
        std::fs::remove_file(files.root().join(rel)).unwrap();

        let mut state = SyncState::new();
        state.mark_synced(rel, Some("\"e\"".into()), Some(2));
        let mut remote = BTreeMap::new();
        remote.insert(rel.to_string(), true);

        let inv = build_inventory(&files, &SyncSelection::all(), &state, &remote, None).unwrap();
        let n = inv.find(rel).expect("★ 已删的文件也必须出现在清单里");
        assert!(!n.local_exists, "应标记本地不存在");
        assert_eq!(n.size, None, "已删的文件没有大小");
    }

    #[test]
    fn state_entries_without_local_file_appear_in_other_bucket() {
        // 哈希存储里的文件被本地删了,也要能看见
        let (_d, files) = setup();
        let rel = "transcript/ab/old.json";
        let mut state = SyncState::new();
        state.mark_synced(rel, Some("\"e\"".into()), Some(5));

        let inv = build_inventory(
            &files,
            &SyncSelection::all(),
            &state,
            &BTreeMap::new(),
            None,
        )
        .unwrap();
        let n = inv.find(rel).expect("状态表里的条目应被列出");
        assert!(!n.local_exists);
    }

    #[test]
    fn empty_store_gives_empty_root() {
        let tmp = tempfile::tempdir().unwrap();
        let files = FileStore::new(tmp.path().join("nothing"));
        std::fs::create_dir_all(files.root()).unwrap();
        let inv = build_inventory(
            &files,
            &SyncSelection::all(),
            &SyncState::new(),
            &BTreeMap::new(),
            None,
        )
        .unwrap();
        assert!(inv.children.is_empty());
    }

    #[test]
    fn explicitly_excluded_flag_only_on_the_marked_node() {
        let (_d, files) = setup();
        let mut sel = SyncSelection::all();
        sel.exclude_path("projects/2026-09-11_高数/audio");

        let inv = build_inventory(&files, &sel, &SyncState::new(), &BTreeMap::new(), None).unwrap();
        let audio = inv.find("projects/2026-09-11_高数/audio").unwrap();
        assert!(audio.explicitly_excluded, "被显式排除的节点应标记");

        let child = inv
            .find("projects/2026-09-11_高数/audio/rec.wav")
            .unwrap();
        assert!(
            !child.explicitly_excluded,
            "子项是被父节点覆盖的,不该标成显式排除"
        );

        let proj = inv.find("projects/2026-09-11_高数").unwrap();
        assert!(!proj.explicitly_excluded);
    }

    #[test]
    fn group_of_classifies_correctly() {
        assert_eq!(group_of("projects/P/summary-brief.md"), "brief");
        assert_eq!(group_of("projects/P/summary-detailed.md"), "detailed");
        assert_eq!(group_of("projects/P/mindmap.mmd"), "mindmap");
        assert_eq!(group_of("projects/P/mindmap-outline.md"), "mindmap");
        assert_eq!(group_of("projects/P/transcript.srt"), "transcript");
        assert_eq!(group_of("projects/P/transcript.json"), "transcript");
        assert_eq!(group_of("projects/P/audio/rec.wav"), "audio");
        assert_eq!(group_of("projects/P/project.json"), "meta");
    }

    /// ★ 回归测试:sync-state 里 manifest 已不认账的条目不该出现在清单里。
    ///
    /// 用户实际报的问题:「同步清单」里挂着一个工程标着"待删除云端",
    /// 但「云端管理」里根本找不到它 —— 两个界面各说各话。
    ///
    /// 原因是 sync-state **不跟 manifest 对账**:条目一旦写入就一直留着。
    /// 手工清理或改名之后,manifest 里已经没有的记录仍然被列进清单,
    /// 而云端也早就没有那个文件了。
    #[test]
    fn stale_state_entries_are_not_listed() {
        use crate::sync::manifest::{Manifest, ManifestEntry};

        let d = tempfile::tempdir().unwrap();
        let files = FileStore::new(d.path());
        std::fs::create_dir_all(files.root()).unwrap();

        // 状态表里有两条,但 manifest 只认其中一条
        let mut st = SyncState::new();
        st.files.insert(
            "projects/P/transcript.md".into(),
            crate::sync::state::FileSyncRecord {
                etag: Some("e1".into()),
                size: Some(10),
                last_synced_at: Some(1),
            },
        );
        st.files.insert(
            "projects/GONE/transcript.md".into(),
            crate::sync::state::FileSyncRecord {
                etag: Some("e2".into()),
                size: Some(10),
                last_synced_at: Some(1),
            },
        );

        let mut m = Manifest::default();
        m.record(
            "projects/P/transcript.md",
            ManifestEntry {
                etag: Some("e1".into()),
                size: Some(10),
                synced_at: 1,
                origin: Some("me".into()),
                deleted_at: None,
            },
        );

        let inv = build_inventory(
            &files,
            &SyncSelection::all(),
            &st,
            &BTreeMap::new(),
            Some(&m),
        )
        .unwrap();

        assert!(
            inv.find("projects/P/transcript.md").is_some(),
            "manifest 认账的条目应保留"
        );
        assert!(
            inv.find("projects/GONE/transcript.md").is_none(),
            "manifest 已不认账的陈旧条目不该出现 —— 这正是用户看到幽灵条目的原因"
        );
    }

    /// 没有 manifest 时不过滤(保持向后兼容)。
    #[test]
    fn without_manifest_state_entries_are_kept() {
        let d = tempfile::tempdir().unwrap();
        let files = FileStore::new(d.path());
        std::fs::create_dir_all(files.root()).unwrap();

        let mut st = SyncState::new();
        st.files.insert(
            "projects/P/transcript.md".into(),
            crate::sync::state::FileSyncRecord {
                etag: Some("e1".into()),
                size: Some(10),
                last_synced_at: Some(1),
            },
        );

        let inv = build_inventory(
            &files,
            &SyncSelection::all(),
            &st,
            &BTreeMap::new(),
            None,
        )
        .unwrap();
        assert!(inv.find("projects/P/transcript.md").is_some());
    }
}
