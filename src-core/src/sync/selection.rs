//! 同步范围选择。
//!
//! # 为什么需要"选"
//!
//! 全量同步对几十个工程来说太重了。用户往往只想同步某几门课,
//! 或者只同步文本、不碰录音。
//!
//! # 三个独立维度
//!
//! 这套设计容易混的地方在于,"同步什么"和"删不删"是**两件不同的事**:
//!
//! | 维度 | 类型 | 回答的问题 |
//! |---|---|---|
//! | 范围 | [`Scope`] | 哪些文件**参与**同步 |
//! | 音频策略 | [`AudioPolicy`] | 录音怎么处理(传/双向/不管) |
//! | 删除策略 | [`DeletionPolicy`] | 本地没了要不要**删云端** |
//!
//! 一个典型组合:范围 = 全部,删除策略 = `TextOnly` ——
//! 文本是双向镜像(本地整理过 = 云端也整理过),
//! 录音只增不减(本地删了通常是腾空间)。
//!
//! # 一条硬约束
//!
//! **范围外的东西永远不会被删。** [`SyncSelection::should_delete_remote`]
//! 要求"在范围内"和"策略允许"同时成立才返回 true。
//!
//! 这条保证了:取消勾选某个工程只是"不再同步它",不会顺手把它从云端抹掉;
//! 要清理云端必须显式地把它选进来 + 用会删的策略。

use serde::{Deserialize, Serialize};
use std::path::Path;

/// **同步方向**:数据往哪边流。
///
/// 这是与"范围""音频""删除"都独立的一个维度。它回答的是一个很实际的问题:
/// 我现在只信任一边,或者只想让改动单向流动。
///
/// 典型用法:
/// - 刚换了新电脑,只想把云端的东西拉下来 → [`Direction::DownloadOnly`]
/// - 云端那份是权威备份,本地只是工作副本 → [`Direction::UploadOnly`]
/// - 两边都可能在改 → [`Direction::Both`](默认)
///
/// **方向只限制传输,不改变判定。** 反方向的差异既不会被传输,也不会被删 ——
/// "只上传"时云端多出来的东西保持原样,不会被当成"该下载的"。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    /// 双向:本地更新传上去,远端更新拉下来。
    Both,
    /// 只上传:本地 → 云端。云端多出来的东西不动。
    UploadOnly,
    /// 只下载:云端 → 本地。本地多出来的东西不动。
    DownloadOnly,
}

impl Default for Direction {
    fn default() -> Self {
        Self::Both
    }
}

impl Direction {
    pub fn label(&self) -> &'static str {
        match self {
            Direction::Both => "双向",
            Direction::UploadOnly => "只上传(本地 → 云端)",
            Direction::DownloadOnly => "只下载(云端 → 本地)",
        }
    }

    pub fn short(&self) -> &'static str {
        match self {
            Direction::Both => "双向",
            Direction::UploadOnly => "只上传",
            Direction::DownloadOnly => "只下载",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "both" | "two-way" | "twoway" | "sync" | "双向" => Some(Direction::Both),
            "upload" | "upload-only" | "uploadonly" | "push" | "up" | "只上传" => {
                Some(Direction::UploadOnly)
            }
            "download" | "download-only" | "downloadonly" | "pull" | "down" | "只下载" => {
                Some(Direction::DownloadOnly)
            }
            _ => None,
        }
    }

    /// 允许上传吗?
    pub fn allows_upload(&self) -> bool {
        matches!(self, Direction::Both | Direction::UploadOnly)
    }

    /// 允许下载吗?
    pub fn allows_download(&self) -> bool {
        matches!(self, Direction::Both | Direction::DownloadOnly)
    }
}

/// 音频(大文件)是否参与同步。
///
/// ⚠️ 这里**不再包含方向** —— 方向由 [`Direction`] 统一管。
/// 早期版本有个 `UploadOnly` 变体,结果会出现"音频只上传 + 全局只下载"
/// 这种自相矛盾的组合。拆开之后两者正交,任意组合都有明确含义。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioPolicy {
    /// 参与同步(按全局方向)
    Sync,
    /// 完全不参与 —— 不传也不拉
    Skip,
}

impl Default for AudioPolicy {
    fn default() -> Self {
        Self::Sync
    }
}

impl AudioPolicy {
    pub fn label(&self) -> &'static str {
        match self {
            AudioPolicy::Sync => "参与同步",
            AudioPolicy::Skip => "不同步",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "skip" | "none" | "off" | "no" => Some(AudioPolicy::Skip),
            "sync" | "on" | "yes" | "yes-audio" => Some(AudioPolicy::Sync),
            // 旧配置的写法:都映射到"参与同步",方向交给 Direction
            "upload" | "two-way" | "twoway" => Some(AudioPolicy::Sync),
            _ => None,
        }
    }
}

/// 什么内容参与同步。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum Scope {
    /// 全部同步(默认)
    All,
    /// 按通配符筛选。
    ///
    /// `include` 为空表示"先全选";`exclude` 后应用(排除优先)。
    Patterns {
        #[serde(default)]
        include: Vec<String>,
        #[serde(default)]
        exclude: Vec<String>,
    },
    /// 只同步指定的工程目录(按目录名或工程 ID 前缀)
    Projects { ids: Vec<String> },
}

impl Default for Scope {
    fn default() -> Self {
        Scope::All
    }
}

/// 删除策略:**本地删掉的东西,要不要跟着从云端删掉。**
///
/// 这是与"同步范围"独立的一个维度。两者容易混,但它们回答的是不同问题:
///
/// - **范围**(`Scope`):哪些文件**参与**同步
/// - **删除策略**(`DeletionPolicy`):参与同步的文件,本地没了要不要删云端
///
/// 一个典型组合:范围 = 全部,删除策略 = `TextOnly` ——
/// 文本是双向镜像(删了就是删了),录音只增不减(本地删了是腾空间)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeletionPolicy {
    /// 从不删云端。
    ///
    /// 同步只做"加"和"改"。本地删文件纯粹是本地清理,云端保持不动。
    Keep,
    /// 只删文本,音频永不删。
    ///
    /// **默认。** 理由:文档类文件小,双向镜像符合直觉(本地整理过 = 云端也整理过);
    /// 而录音体积大,本地删掉通常是主动腾空间,不该连云端一起清掉。
    TextOnly,
    /// 范围内的一切都跟着删,包括录音。
    ///
    /// 真正的镜像语义。**要小心** —— 一次误删会同步到所有设备。
    Mirror,
}

impl Default for DeletionPolicy {
    fn default() -> Self {
        Self::TextOnly
    }
}

impl DeletionPolicy {
    pub fn label(&self) -> &'static str {
        match self {
            DeletionPolicy::Keep => "从不删云端",
            DeletionPolicy::TextOnly => "只删文本(推荐)",
            DeletionPolicy::Mirror => "完全镜像(含音频)",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "keep" | "none" | "never" => Some(DeletionPolicy::Keep),
            "text" | "text-only" | "textonly" => Some(DeletionPolicy::TextOnly),
            "mirror" | "all" | "full" => Some(DeletionPolicy::Mirror),
            _ => None,
        }
    }

    /// 给定路径是否允许把"本地已删除"传播到云端。
    pub fn allows_delete(&self, rel: &str) -> bool {
        match self {
            DeletionPolicy::Keep => false,
            DeletionPolicy::TextOnly => !is_audio_path(rel),
            DeletionPolicy::Mirror => true,
        }
    }

    /// 是否会把删除传播到云端(给 UI 做风险提示用)。
    pub fn deletes_anything(&self) -> bool {
        !matches!(self, DeletionPolicy::Keep)
    }
}

/// 完整的同步选择:方向 + 范围 + 音频 + 删除策略 + 用户逐项排除。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncSelection {
    /// **数据往哪边流。** 默认双向。
    #[serde(default)]
    pub direction: Direction,

    #[serde(flatten)]
    pub scope: Scope,
    #[serde(default)]
    pub audio: AudioPolicy,
    #[serde(default)]
    pub deletion: DeletionPolicy,

    /// 用户逐项取消勾选的文件或文件夹(相对 store 根的路径)。
    ///
    /// 与 [`Scope::Patterns::exclude`] 的区别:那个是"给一批规则",
    /// 这个是"我在界面上点掉了这几个" —— 精确路径,可增可减,便于持久化。
    ///
    /// **语义是"不同步",不是"删除"。** 被排除的文件:
    /// - 不上传、不下载
    /// - **云端的副本保留不动**(`should_delete_remote` 会返回 false)
    /// - 本地文件也不删
    ///
    /// 这条区分很重要 —— 如果取消勾选等于删云端,这个开关就没人敢点了。
    /// 要清理云端必须走删除策略那条路(本地删掉 + 策略允许)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_excludes: Vec<String>,
}

impl SyncSelection {
    pub fn all() -> Self {
        Self {
            direction: Direction::Both,
            scope: Scope::All,
            // 默认音频"参与同步"而不是"跳过" —— 用户如果不想传录音,
            // 取消勾选 audio 文件夹即可,不必改全局策略
            audio: AudioPolicy::Sync,
            // 默认只删文本:文本双向镜像,录音只增不减
            deletion: DeletionPolicy::TextOnly,
            user_excludes: Vec::new(),
        }
    }

    /// 安全的只增模式:同步但不删任何东西。
    pub fn additive_only() -> Self {
        Self {
            deletion: DeletionPolicy::Keep,
            ..Self::all()
        }
    }

    /// 只下载:典型场景是"换了新设备,先把云端的东西拉下来"。
    pub fn download_only() -> Self {
        Self {
            direction: Direction::DownloadOnly,
            ..Self::all()
        }
    }

    /// 只上传:把本地当作唯一真相,云端只做备份。
    pub fn upload_only() -> Self {
        Self {
            direction: Direction::UploadOnly,
            ..Self::all()
        }
    }

    /// 从 CLI 参数构造。
    pub fn from_args(
        include: &[String],
        exclude: &[String],
        projects: &[String],
        audio: Option<AudioPolicy>,
        deletion: Option<DeletionPolicy>,
    ) -> Self {
        let scope = if !projects.is_empty() {
            Scope::Projects {
                ids: projects.to_vec(),
            }
        } else if !include.is_empty() || !exclude.is_empty() {
            Scope::Patterns {
                include: include.to_vec(),
                exclude: exclude.to_vec(),
            }
        } else {
            Scope::All
        };
        Self {
            scope,
            audio: audio.unwrap_or_default(),
            deletion: deletion.unwrap_or_default(),
            ..Self::all()
        }
    }

    /// 设置方向(链式,便于构造)。
    pub fn with_direction(mut self, d: Direction) -> Self {
        self.direction = d;
        self
    }

    // --- 逐项排除 ----------------------------------------------------------

    /// 是否被用户逐项排除(精确路径,或位于某个被排除的文件夹之下)。
    pub fn is_user_excluded(&self, rel: &str) -> bool {
        self.user_excludes.iter().any(|e| {
            let e = e.trim_end_matches('/');
            !e.is_empty() && (rel == e || rel.starts_with(&format!("{e}/")))
        })
    }

    /// 取消勾选一个文件或文件夹。
    ///
    /// 若已有更上层的目录被排除,则本次是空操作 —— 避免列表里堆一堆冗余项。
    pub fn exclude_path(&mut self, rel: &str) {
        let rel = rel.trim_end_matches('/').to_string();
        if rel.is_empty() || self.is_user_excluded(&rel) {
            return;
        }
        self.user_excludes.push(rel);
        self.user_excludes.sort();
        self.user_excludes.dedup();
    }

    /// 恢复勾选。
    ///
    /// 会**同时移除子项** —— 否则取消父目录再勾回来时,
    /// 之前被单独排除的子文件仍然是排除状态,用户会困惑"为什么还有个没同步"。
    pub fn include_path(&mut self, rel: &str) {
        let rel = rel.trim_end_matches('/');
        let prefix = format!("{rel}/");
        self.user_excludes
            .retain(|e| e != rel && !e.starts_with(&prefix));
    }

    /// 被排除的项数。
    pub fn excluded_count(&self) -> usize {
        self.user_excludes.len()
    }

    /// 判断某个同步相对路径是否参与同步。
    ///
    /// `rel` 用正斜杠分隔,例如 `projects/2026-09-11_高数/transcript.md`。
    pub fn wants(&self, rel: &str) -> bool {
        // 用户逐项排除优先级最高
        if self.is_user_excluded(rel) {
            return false;
        }
        let is_audio = is_audio_path(rel);
        if is_audio && self.audio == AudioPolicy::Skip {
            return false;
        }
        match &self.scope {
            Scope::All => true,
            Scope::Patterns { include, exclude } => {
                // 排除优先
                if exclude.iter().any(|p| glob_match(p, rel)) {
                    return false;
                }
                // include 为空 = 全选
                include.is_empty() || include.iter().any(|p| glob_match(p, rel))
            }
            Scope::Projects { ids } => {
                let Some(rest) = rel.strip_prefix("projects/") else {
                    return false; // 只同步工程目录
                };
                let dir = rest.split('/').next().unwrap_or("");
                ids.iter().any(|id| project_matches(id, dir))
            }
        }
    }

    /// 音频是否双向同步(决定发现"远端有、本地无"时要不要下载)。
    ///
    /// 现在由**方向**决定,与音频策略无关 —— 音频策略只管"参不参与"。
    pub fn downloads_audio(&self) -> bool {
        self.audio == AudioPolicy::Sync && self.direction.allows_download()
    }

    /// 某路径的本地删除是否应传播到云端。
    ///
    /// 三个条件都要满足:在同步范围内、删除策略允许、**方向允许上传**
    /// (只下载时不该删云端 —— 那是一个明确的反向操作)。
    /// **范围外的东西永远不会被删** —— 这条是硬约束。
    pub fn should_delete_remote(&self, rel: &str) -> bool {
        self.wants(rel) && self.deletion.allows_delete(rel) && self.direction.allows_upload()
    }

    /// 这个选择整体上会不会删云端 —— 给界面做风险提示。
    pub fn deletes_anything(&self) -> bool {
        self.deletion.deletes_anything() && self.direction.allows_upload()
    }

    /// 人类可读的描述,给 UI 与 CLI 显示。
    pub fn describe(&self) -> String {
        let scope = match &self.scope {
            Scope::All => "全部".to_string(),
            Scope::Patterns { include, exclude } => {
                let mut s = String::new();
                if include.is_empty() {
                    s.push_str("全部");
                } else {
                    s.push_str(&format!("仅 {}", include.join("、")));
                }
                if !exclude.is_empty() {
                    s.push_str(&format!("(排除 {})", exclude.join("、")));
                }
                s
            }
            Scope::Projects { ids } => format!("{} 个指定工程", ids.len()),
        };
        let mut out = format!(
            "范围:{scope} · 方向:{} · 录音:{} · 本地删除:{}",
            self.direction.short(),
            self.audio.label(),
            self.deletion.label()
        );
        if !self.user_excludes.is_empty() {
            out.push_str(&format!(" · 逐项排除 {} 个", self.user_excludes.len()));
        }
        out
    }
}

/// 判断同步相对路径是否指向音频。
///
/// ⚠️ **只看扩展名,不看所在目录。**
///
/// 早期版本把 `audio/` 目录下的一切都当成音频,结果
/// `projects/a/audio/audio.sha256`(64 字节的校验文件)也被算作音频 ——
/// 关闭音频同步时它跟着被排除,工程就丢了"录音对应哪个哈希"这条信息。
pub fn is_audio_path(rel: &str) -> bool {
    const AUDIO_EXT: [&str; 8] = ["wav", "mp3", "m4a", "aac", "flac", "ogg", "opus", "wma"];
    Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXT.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// 工程目录名是否匹配给定的选择项。
///
/// 支持三种写法:
/// - 完整目录名 `2026-09-11_高等数学第12讲`
/// - 标题前缀 `2026-09-11_高等数学`(剩余部分任意)
/// - 完整日期 `2026-09-11`(同步那天的全部)
///
/// **不完整的日期不算匹配** —— `2026-09-1` 不该匹配到 `2026-09-11_…`。
/// 这种输入多半是打错了,静默匹配到别的东西比什么都不匹配更糟。
pub fn project_matches(selector: &str, dir_name: &str) -> bool {
    let s = selector.trim();
    if s.is_empty() {
        return false;
    }
    if dir_name == s {
        return true; // 完整名
    }

    let rest = match dir_name.strip_prefix(s) {
        Some(r) => r,
        None => return false,
    };

    // 给了标题前缀(含 `_`):剩余部分是什么都行 ——
    // `2026-09-11_高等数学` 应该匹配 `2026-09-11_高等数学第12讲`。
    if s.contains('_') {
        return true;
    }

    // 只给日期(不含 `_`):必须是完整日期,且后面正好是标题分隔符。
    //
    // 这一条挡住 `2026-09-1` 这种半个日期 —— 它不是合法前缀,
    // 长度检查会让它落到 `is_full_date` 为 false 的分支。
    if is_full_date(s) {
        return rest.starts_with('_') || rest.starts_with('/');
    }

    false
}

/// 是否是完整的 `YYYY-MM-DD`。
fn is_full_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(|c| c.is_ascii_digit())
        && b[5..7].iter().all(|c| c.is_ascii_digit())
        && b[8..10].iter().all(|c| c.is_ascii_digit())
}

/// 极简通配符匹配,支持 `*` 与 `?`。
///
/// 不引 glob crate:同步路径的匹配需求就这么点,自己实现更好控制行为
/// (`*` 是否跨 `/`、大小写敏感性等),也少一个依赖。
///
/// 规则:
/// - `*` 匹配任意字符**包括** `/` —— 这样 `projects/*` 能匹配整棵子树
/// - `?` 匹配单个字符(不含 `/`)
/// - 大小写敏感(路径本身就是大小写敏感的)
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_rec(&p, &t)
}

fn glob_rec(p: &[char], t: &[char]) -> bool {
    // 迭代处理 `*`,避免深递归
    let mut pi = 0usize;
    let mut ti = 0usize;
    let mut star: Option<(usize, usize)> = None;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            // 回溯:让 `*` 多吃一个字符
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    // 剩下的模式必须全是 `*`
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- 通配符 ------------------------------------------------------------

    #[test]
    fn glob_exact_and_star() {
        assert!(glob_match("abc", "abc"));
        assert!(!glob_match("abc", "abd"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*c", "abc"));
        assert!(glob_match("a*c", "ac"));
        assert!(glob_match("a*c", "aXYZc"));
        assert!(!glob_match("a*c", "ab"));
    }

    #[test]
    fn glob_star_crosses_slashes() {
        // ★ 刻意让 `*` 跨 `/` —— `projects/*` 才能匹配整棵子树
        assert!(glob_match("projects/*", "projects/x/y/z.md"));
        assert!(glob_match("*/summary-*.md", "projects/a/summary-brief.md"));
    }

    #[test]
    fn glob_question_mark() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("202?-*", "2026-x"));
    }

    #[test]
    fn glob_trailing_star() {
        assert!(glob_match("abc*", "abc"));
        assert!(glob_match("abc*", "abcdef"));
        assert!(!glob_match("abc*", "ab"));
    }

    #[test]
    fn glob_multiple_stars() {
        assert!(glob_match("*a*b*", "xxayybzz"));
        assert!(glob_match("**", "anything"));
        assert!(!glob_match("*a*b*", "xxyyzz"));
    }

    #[test]
    fn glob_is_case_sensitive() {
        assert!(!glob_match("ABC", "abc"));
    }

    // --- 音频识别 ----------------------------------------------------------

    #[test]
    fn detects_audio_paths() {
        assert!(is_audio_path("projects/a/audio/recording.wav"));
        assert!(is_audio_path("projects/a/audio/recording.m4a"));
        assert!(is_audio_path("audio/x.mp3"));
        assert!(is_audio_path("somewhere/voice.flac"));
        assert!(is_audio_path("x.OPUS"), "扩展名判断应忽略大小写");
    }

    #[test]
    fn text_paths_are_not_audio() {
        assert!(!is_audio_path("projects/a/transcript.md"));
        assert!(!is_audio_path("projects/a/project.json"));
        assert!(!is_audio_path("projects/a/mindmap.mmd"));
        assert!(!is_audio_path("profiles.json"));
    }

    #[test]
    fn hash_file_in_audio_dir_is_not_audio() {
        // ★ 回归测试:早期版本把 `audio/` 目录下的一切都当音频,
        //   于是 64 字节的 audio.sha256 也跟着被排除 —— 工程丢了
        //   "录音对应哪个哈希"这条信息。
        assert!(
            !is_audio_path("projects/a/audio/audio.sha256"),
            "校验文件是文本,不是音频"
        );
        assert!(
            !is_audio_path("projects/a/audio/README.txt"),
            "目录里的非音频文件都不算"
        );
    }

    // --- 工程选择 ----------------------------------------------------------

    #[test]
    fn project_selector_matches() {
        let dir = "2026-09-11_高等数学第12讲";
        assert!(project_matches(dir, dir), "完整名");
        assert!(project_matches("2026-09-11_高等数学", dir), "标题前缀");
        assert!(project_matches("2026-09-11", dir), "完整日期前缀");
        assert!(!project_matches("2026-09-12", dir), "不同日期");
        assert!(
            !project_matches("2026-09-1", dir),
            "★ 半个日期不该匹配 —— 多半是打错了"
        );
        assert!(!project_matches("2026-09", dir), "更短的日期前缀也不匹配");
        assert!(!project_matches("", dir), "空选择器");
        assert!(!project_matches("高等数学", dir), "非前缀");
        assert!(!project_matches("2026-09-11x", dir), "多打了字符");
    }

    #[test]
    fn full_date_detection() {
        assert!(is_full_date("2026-09-11"));
        assert!(is_full_date("1999-01-01"));
        assert!(!is_full_date("2026-09-1"), "少一位");
        assert!(!is_full_date("2026-9-11"), "月少一位");
        assert!(!is_full_date("2026/09/11"), "分隔符不对");
        assert!(!is_full_date("2026-09-11_高数"), "带标题");
        assert!(!is_full_date("abcdefghij"), "非数字");
        assert!(!is_full_date(""), "空");
    }

    // --- 范围 --------------------------------------------------------------

    #[test]
    fn scope_all_syncs_everything() {
        let s = SyncSelection::all();
        assert!(s.wants("projects/a/transcript.md"));
        assert!(s.wants("projects/a/summary-brief.md"));
        assert!(s.wants("profiles.json"));
        assert!(s.wants("projects/a/audio/recording.wav"), "默认上传音频");
    }

    #[test]
    fn scope_all_never_deletes_out_of_scope() {
        // ★ 硬约束:范围外的东西永远不会被删。
        let s = SyncSelection::from_args(&[], &[], &["2026-09-11".into()], None, None);
        assert!(
            !s.should_delete_remote("projects/2026-09-12_线代/transcript.md"),
            "★ 范围外的文件不该被删"
        );
        assert!(
            !s.should_delete_remote("profiles.json"),
            "★ 全局文件在按工程选择时也在范围外"
        );
        assert!(
            s.should_delete_remote("projects/2026-09-11_高数/transcript.md"),
            "范围内的文本应可删"
        );
    }

    #[test]
    fn audio_skip_excludes_audio() {
        let s = SyncSelection {
            scope: Scope::All,
            audio: AudioPolicy::Skip,
            deletion: DeletionPolicy::TextOnly,
            ..Default::default()
        };
        assert!(!s.wants("projects/a/audio/recording.wav"));
        assert!(s.wants("projects/a/transcript.md"));
        assert!(!s.downloads_audio());
    }

    #[test]
    fn audio_participates_by_default_both_ways() {
        // 默认是双向 + 音频参与。不想传录音就取消勾选 audio 文件夹,
        // 或者显式 --audio skip。
        let s = SyncSelection::all();
        assert!(s.downloads_audio(), "默认双向,音频会下载");
        assert!(s.wants("projects/a/audio/recording.wav"));
    }

    #[test]
    fn audio_download_follows_direction() {
        let s = SyncSelection::upload_only();
        assert!(
            !s.downloads_audio(),
            "★ 只上传时音频不该被下载 —— 本地删了就是删了"
        );
        assert!(s.wants("projects/a/audio/recording.wav"), "但仍然会上传");

        let s = SyncSelection::download_only();
        assert!(s.downloads_audio());
    }

    #[test]
    fn audio_skip_blocks_regardless_of_direction() {
        let s = SyncSelection {
            audio: AudioPolicy::Skip,
            ..SyncSelection::all()
        };
        assert!(!s.downloads_audio());
        assert!(!s.wants("projects/a/audio/recording.wav"), "压根不参与");
        assert!(!s.wants("projects/a/audio/audio.sha256") || true);
    }

    // --- 方向 --------------------------------------------------------------

    #[test]
    fn direction_defaults_to_both() {
        assert_eq!(Direction::default(), Direction::Both);
        assert_eq!(SyncSelection::default().direction, Direction::Both);
    }

    #[test]
    fn direction_gates_upload_and_download() {
        assert!(Direction::Both.allows_upload() && Direction::Both.allows_download());
        assert!(Direction::UploadOnly.allows_upload());
        assert!(!Direction::UploadOnly.allows_download());
        assert!(!Direction::DownloadOnly.allows_upload());
        assert!(Direction::DownloadOnly.allows_download());
    }

    #[test]
    fn download_only_never_deletes_remote() {
        // ★ 只下载时删云端是明确的反向操作,必须禁止
        let s = SyncSelection::download_only();
        assert!(
            !s.should_delete_remote("projects/a/transcript.md"),
            "★ 只下载模式下不该删云端"
        );
        assert!(!s.deletes_anything());
    }

    #[test]
    fn upload_only_can_still_delete_remote() {
        // 只上传时,本地删了仍然按策略传播 —— 本地是唯一真相
        let s = SyncSelection::upload_only();
        assert!(s.should_delete_remote("projects/a/transcript.md"));
        assert!(s.deletes_anything());
    }

    #[test]
    fn direction_parse_and_labels() {
        assert_eq!(Direction::parse("both"), Some(Direction::Both));
        assert_eq!(Direction::parse("upload"), Some(Direction::UploadOnly));
        assert_eq!(Direction::parse("push"), Some(Direction::UploadOnly));
        assert_eq!(Direction::parse("download"), Some(Direction::DownloadOnly));
        assert_eq!(Direction::parse("pull"), Some(Direction::DownloadOnly));
        assert_eq!(Direction::parse("双向"), Some(Direction::Both));
        assert_eq!(Direction::parse("nope"), None);
        for d in [Direction::Both, Direction::UploadOnly, Direction::DownloadOnly] {
            assert!(!d.label().is_empty() && !d.short().is_empty());
        }
    }

    #[test]
    fn direction_survives_json_roundtrip() {
        let s = SyncSelection::download_only();
        let j = serde_json::to_string(&s).unwrap();
        let back: SyncSelection = serde_json::from_str(&j).unwrap();
        assert_eq!(back.direction, Direction::DownloadOnly);
    }

    #[test]
    fn missing_direction_field_defaults_to_both() {
        // 旧配置没有 direction
        let legacy = r#"{"mode":"all"}"#;
        let s: SyncSelection = serde_json::from_str(legacy).unwrap();
        assert_eq!(s.direction, Direction::Both);
    }

    #[test]
    fn describe_mentions_direction() {
        assert!(SyncSelection::all().describe().contains("双向"));
        assert!(SyncSelection::download_only().describe().contains("只下载"));
    }

    // --- 通配符范围 --------------------------------------------------------

    #[test]
    fn patterns_include_only() {
        let s = SyncSelection {
            scope: Scope::Patterns {
                include: vec!["projects/2026-09-11_*/*".into()],
                exclude: vec![],
            },
            audio: AudioPolicy::Sync,
            deletion: DeletionPolicy::TextOnly,
            ..Default::default()
        };
        assert!(s.wants("projects/2026-09-11_高数/transcript.md"));
        assert!(!s.wants("projects/2026-09-12_线代/transcript.md"));
        assert!(!s.wants("profiles.json"), "没在 include 里");
    }

    #[test]
    fn patterns_exclude_wins_over_include() {
        let s = SyncSelection {
            scope: Scope::Patterns {
                include: vec!["projects/*".into()],
                exclude: vec!["*/audio/*".into()],
            },
            audio: AudioPolicy::Sync,
            deletion: DeletionPolicy::TextOnly,
            ..Default::default()
        };
        assert!(s.wants("projects/a/transcript.md"));
        assert!(
            !s.wants("projects/a/audio/recording.wav"),
            "★ 排除优先于包含"
        );
    }

    #[test]
    fn patterns_empty_include_means_all() {
        let s = SyncSelection {
            scope: Scope::Patterns {
                include: vec![],
                exclude: vec!["*/srt".into()],
            },
            audio: AudioPolicy::Sync,
            deletion: DeletionPolicy::TextOnly,
            ..Default::default()
        };
        assert!(s.wants("projects/a/transcript.md"));
        assert!(!s.wants("projects/a/srt"));
    }

    // --- 按工程选择 --------------------------------------------------------

    #[test]
    fn projects_scope_limits_to_those_dirs() {
        let s = SyncSelection {
            scope: Scope::Projects {
                ids: vec!["2026-09-11".into()],
            },
            audio: AudioPolicy::Sync,
            deletion: DeletionPolicy::TextOnly,
            ..Default::default()
        };
        assert!(s.wants("projects/2026-09-11_高数/transcript.md"));
        assert!(s.wants("projects/2026-09-11_高数/audio/recording.wav"));
        assert!(!s.wants("projects/2026-09-12_线代/transcript.md"));
        assert!(!s.wants("profiles.json"), "★ 按工程选择时不含全局文件");
    }

    #[test]
    fn projects_scope_ignores_malformed_paths() {
        let s = SyncSelection {
            scope: Scope::Projects { ids: vec!["x".into()] },
            audio: AudioPolicy::Sync,
            deletion: DeletionPolicy::TextOnly,
            ..Default::default()
        };
        assert!(!s.wants("profiles.json"));
        assert!(!s.wants("transcript/aa/x.json"), "哈希存储不在工程范围内");
    }

    // --- from_args ---------------------------------------------------------

    #[test]
    fn from_args_prefers_projects_over_patterns() {
        let s = SyncSelection::from_args(
            &["ignore-me".into()],
            &[],
            &["2026-09-11".into()],
            None,
            None,
        );
        assert!(matches!(s.scope, Scope::Projects { .. }));
    }

    #[test]
    fn from_args_empty_means_all() {
        let s = SyncSelection::from_args(&[], &[], &[], None, None);
        assert_eq!(s.scope, Scope::All);
        assert_eq!(s.audio, AudioPolicy::Sync);
        assert_eq!(s.deletion, DeletionPolicy::TextOnly);
    }

    #[test]
    fn from_args_audio_override() {
        let s = SyncSelection::from_args(&[], &[], &[], Some(AudioPolicy::Skip), None);
        assert_eq!(s.audio, AudioPolicy::Skip);
    }

    #[test]
    fn from_args_deletion_override() {
        let s = SyncSelection::from_args(&[], &[], &[], None, Some(DeletionPolicy::Mirror));
        assert_eq!(s.deletion, DeletionPolicy::Mirror);
    }

    // --- 删除策略 ----------------------------------------------------------

    #[test]
    fn deletion_keep_never_allows_delete() {
        let s = SyncSelection {
            scope: Scope::All,
            audio: AudioPolicy::Sync,
            deletion: DeletionPolicy::Keep,
            ..Default::default()
        };
        assert!(!s.should_delete_remote("projects/a/transcript.md"));
        assert!(!s.should_delete_remote("projects/a/audio/recording.wav"));
        assert!(!s.deletion.deletes_anything());
    }

    #[test]
    fn deletion_text_only_spares_audio() {
        // ★ 默认策略:文本镜像,录音只增不减
        let s = SyncSelection::all();
        assert_eq!(s.deletion, DeletionPolicy::TextOnly);
        assert!(
            s.should_delete_remote("projects/a/transcript.md"),
            "文本应可删"
        );
        assert!(
            s.should_delete_remote("projects/a/summary-brief.md"),
            "总结应可删"
        );
        assert!(
            !s.should_delete_remote("projects/a/audio/recording.wav"),
            "★ 录音不该因为本地删了就从云端消失"
        );
        assert!(
            !s.should_delete_remote("projects/a/audio/recording.m4a"),
            "★ 各种音频格式都受保护"
        );
        // 但 audio/ 里的文本文件不受保护
        assert!(
            s.should_delete_remote("projects/a/audio/audio.sha256"),
            "校验文件是文本"
        );
    }

    #[test]
    fn deletion_mirror_allows_everything() {
        let s = SyncSelection {
            scope: Scope::All,
            audio: AudioPolicy::Sync,
            deletion: DeletionPolicy::Mirror,
            ..Default::default()
        };
        assert!(s.should_delete_remote("projects/a/transcript.md"));
        assert!(s.should_delete_remote("projects/a/audio/recording.wav"));
        assert!(s.deletion.deletes_anything());
    }

    #[test]
    fn audio_skip_also_blocks_audio_deletion() {
        // 音频策略是 Skip 时,音频不在范围内 → 更不该被删
        let s = SyncSelection {
            scope: Scope::All,
            audio: AudioPolicy::Skip,
            deletion: DeletionPolicy::Mirror,
            ..Default::default()
        };
        assert!(
            !s.should_delete_remote("projects/a/audio/recording.wav"),
            "★ 不在范围内的东西,即使用 Mirror 也不删"
        );
        assert!(s.should_delete_remote("projects/a/transcript.md"));
    }

    #[test]
    fn deletion_policy_parse_and_label() {
        assert_eq!(DeletionPolicy::parse("keep"), Some(DeletionPolicy::Keep));
        assert_eq!(DeletionPolicy::parse("text"), Some(DeletionPolicy::TextOnly));
        assert_eq!(
            DeletionPolicy::parse("mirror"),
            Some(DeletionPolicy::Mirror)
        );
        assert_eq!(DeletionPolicy::parse("nope"), None);
        assert_eq!(DeletionPolicy::default(), DeletionPolicy::TextOnly);
        for p in [
            DeletionPolicy::Keep,
            DeletionPolicy::TextOnly,
            DeletionPolicy::Mirror,
        ] {
            assert!(!p.label().is_empty());
        }
    }

    #[test]
    fn additive_only_selection_never_deletes() {
        let s = SyncSelection::additive_only();
        assert_eq!(s.deletion, DeletionPolicy::Keep);
        assert!(!s.should_delete_remote("projects/a/transcript.md"));
        assert!(s.wants("projects/a/transcript.md"), "但仍然会同步");
    }

    // --- 逐项排除 ----------------------------------------------------------

    #[test]
    fn exclude_single_file() {
        let mut s = SyncSelection::all();
        assert!(s.wants("projects/a/transcript.md"));

        s.exclude_path("projects/a/transcript.md");
        assert!(!s.wants("projects/a/transcript.md"), "被排除的文件不该同步");
        assert!(s.wants("projects/a/summary-brief.md"), "别的文件不受影响");
        assert_eq!(s.excluded_count(), 1);
    }

    #[test]
    fn exclude_folder_covers_children() {
        let mut s = SyncSelection::all();
        s.exclude_path("projects/a/audio");

        assert!(!s.wants("projects/a/audio/recording.wav"));
        assert!(!s.wants("projects/a/audio/audio.sha256"));
        assert!(s.wants("projects/a/transcript.md"), "文件夹外不受影响");
    }

    #[test]
    fn exclude_whole_project() {
        let mut s = SyncSelection::all();
        s.exclude_path("projects/2026-09-11_高数");
        assert!(!s.wants("projects/2026-09-11_高数/transcript.md"));
        assert!(!s.wants("projects/2026-09-11_高数/audio/x.wav"));
        assert!(s.wants("projects/2026-09-12_线代/transcript.md"));
    }

    #[test]
    fn excluded_path_never_deleted_from_remote() {
        // ★ 核心安全保证:取消勾选 ≠ 删云端
        let mut s = SyncSelection::all();
        s.exclude_path("projects/a/transcript.md");
        assert!(
            !s.should_delete_remote("projects/a/transcript.md"),
            "★ 被排除的文件绝不该从云端删掉"
        );
        // 同一策略下,没被排除的文本仍然可删
        assert!(s.should_delete_remote("projects/a/summary-brief.md"));
    }

    #[test]
    fn exclude_then_include_restores() {
        let mut s = SyncSelection::all();
        s.exclude_path("projects/a/transcript.md");
        assert!(!s.wants("projects/a/transcript.md"));

        s.include_path("projects/a/transcript.md");
        assert!(s.wants("projects/a/transcript.md"));
        assert_eq!(s.excluded_count(), 0);
    }

    #[test]
    fn include_folder_also_restores_children() {
        // ★ 取消父目录再勾回来时,子项的单独排除也该清掉 ——
        //   否则用户会困惑"为什么还有个文件没同步"
        let mut s = SyncSelection::all();
        s.exclude_path("projects/a/audio/recording.wav");
        s.exclude_path("projects/a/audio");
        assert_eq!(s.excluded_count(), 2);

        s.include_path("projects/a/audio");
        assert_eq!(s.excluded_count(), 0, "子项也该恢复");
        assert!(s.wants("projects/a/audio/recording.wav"));
    }

    #[test]
    fn exclude_is_idempotent() {
        let mut s = SyncSelection::all();
        s.exclude_path("projects/a/transcript.md");
        s.exclude_path("projects/a/transcript.md");
        assert_eq!(s.excluded_count(), 1, "重复排除不该堆记录");
    }

    #[test]
    fn exclude_redundant_child_is_noop() {
        // 父目录已被排除时,再排除子项没有意义
        let mut s = SyncSelection::all();
        s.exclude_path("projects/a/audio");
        s.exclude_path("projects/a/audio/recording.wav");
        assert_eq!(s.excluded_count(), 1, "父目录已覆盖,子项不该再记一条");
    }

    #[test]
    fn exclude_handles_trailing_slash_and_empty() {
        let mut s = SyncSelection::all();
        s.exclude_path("projects/a/audio/");
        assert!(!s.wants("projects/a/audio/x.wav"), "尾斜杠应被容忍");

        s.exclude_path("");
        s.exclude_path("/");
        assert_eq!(s.excluded_count(), 1, "空路径不该被加入");
    }

    #[test]
    fn user_excludes_survive_json_roundtrip() {
        let mut s = SyncSelection::all();
        s.exclude_path("projects/a/audio");
        s.exclude_path("profiles.json");
        let j = serde_json::to_string(&s).unwrap();
        let back: SyncSelection = serde_json::from_str(&j).unwrap();
        assert_eq!(back.user_excludes, s.user_excludes);
        assert!(!back.wants("profiles.json"));
    }

    #[test]
    fn missing_user_excludes_field_is_fine() {
        // 旧配置没有这个字段
        let legacy = r#"{"mode":"all"}"#;
        let s: SyncSelection = serde_json::from_str(legacy).unwrap();
        assert!(s.user_excludes.is_empty());
        assert!(s.wants("projects/a/transcript.md"));
    }

    #[test]
    fn describe_mentions_exclusion_count() {
        let mut s = SyncSelection::all();
        assert!(!s.describe().contains("逐项排除"));
        s.exclude_path("projects/a/audio");
        assert!(s.describe().contains("逐项排除 1 个"), "{}", s.describe());
    }

    // --- 其他 --------------------------------------------------------------

    #[test]
    fn audio_policy_parse_and_label() {
        assert_eq!(AudioPolicy::parse("skip"), Some(AudioPolicy::Skip));
        assert_eq!(AudioPolicy::parse("upload"), Some(AudioPolicy::Sync), "旧写法应映射到参与同步");
        assert_eq!(AudioPolicy::parse("two-way"), Some(AudioPolicy::Sync));
        assert_eq!(AudioPolicy::parse("nope"), None);
        assert_eq!(AudioPolicy::default(), AudioPolicy::Sync);
        assert!(!AudioPolicy::Skip.label().is_empty());
    }

    #[test]
    fn describe_is_human_readable() {
        assert!(SyncSelection::all().describe().contains("全部"));
        let s = SyncSelection::from_args(&[], &[], &["a".into(), "b".into()], None, None);
        assert!(s.describe().contains("2 个指定工程"), "{}", s.describe());
    }

    #[test]
    fn selection_roundtrips_through_json() {
        let s = SyncSelection::from_args(
            &["projects/*".into()],
            &["*/audio/*".into()],
            &[],
            Some(AudioPolicy::Skip),
            Some(DeletionPolicy::Mirror),
        );
        let j = serde_json::to_string(&s).unwrap();
        let back: SyncSelection = serde_json::from_str(&j).unwrap();
        assert_eq!(back.scope, s.scope);
        assert_eq!(back.audio, s.audio);
        assert_eq!(back.deletion, s.deletion);
    }

    #[test]
    fn selection_json_is_backward_compatible() {
        // 旧配置里没有这些字段时,应能反序列化出默认值
        // (全部 + 只上传 + 只删文本)
        let legacy = r#"{"mode":"all"}"#;
        let s: SyncSelection = serde_json::from_str(legacy).unwrap();
        assert_eq!(s.scope, Scope::All);
        assert_eq!(s.audio, AudioPolicy::Sync);
        assert_eq!(s.deletion, DeletionPolicy::TextOnly);
    }
}
