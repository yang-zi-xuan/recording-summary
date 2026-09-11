//! 核心数据契约 —— 存储层的原子单位与跨模块共享类型。
//!
//! 见 `docs/技术方案.md` §4.1。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// 音频引用
// ---------------------------------------------------------------------------

/// 指向一段待处理音频。可以是外部文件,也可以是采集落盘的临时文件。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioRef {
    pub path: PathBuf,
    /// 音频时长(毫秒)。未知时为 None,由解码阶段填充。
    pub duration_ms: Option<u64>,
    /// 内容哈希(sha256 十六进制),同时用作会话 ID 与缓存键前缀。
    pub content_hash: Option<String>,
    /// 采集来源。影响说话人区分的可信度(见技术方案 §7.1 / §14.8)。
    pub source: AudioSourceKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioSourceKind {
    /// 导入的外部音频/视频文件(手机、录音笔等)
    File,
    /// 双轨采集:系统声音 + 麦克风独立轨道
    DualTrack,
    /// 仅麦克风采集
    Mic,
}

impl AudioSourceKind {
    /// 双轨采集时说话人是"直接知道"的,单文件只能靠统计推断。
    pub fn speakers_are_reliable(self) -> bool {
        matches!(self, AudioSourceKind::DualTrack)
    }
}

// ---------------------------------------------------------------------------
// 转写结果
// ---------------------------------------------------------------------------

/// 转写段落 —— 存储层的原子单位。
///
/// 注意:`text` 是唯一真相源的一部分,展示用的时间戳前缀、说话人前缀
/// 一律由视图层派生,**不要**把它们写回这个结构。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Segment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    /// None 表示未做说话人区分
    pub speaker_id: Option<u32>,
    /// 是否多人重叠语音(见技术方案 §7.4)
    pub overlapped: bool,
}

impl Segment {
    pub fn duration_ms(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }

    pub fn new(start_ms: u64, end_ms: u64, text: impl Into<String>) -> Self {
        Self {
            start_ms,
            end_ms,
            text: text.into(),
            speaker_id: None,
            overlapped: false,
        }
    }
}

/// 一次转写的完整结果。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transcript {
    pub engine: String,
    pub model: String,
    /// 记录实际使用的后端,便于跨机器排查结果差异。
    pub backend: Backend,
    /// 转写时未做说话人区分则为 None
    pub backend_diarize: Option<Backend>,
    pub language: Option<String>,
    pub segments: Vec<Segment>,
    /// 未合并的纯文本,供复制与 LLM 输入使用。
    pub raw_text: String,
    pub duration_ms: u64,
    pub diarize: Option<DiarizeInfo>,
}

impl Transcript {
    /// 是否存在说话人信息
    pub fn has_speakers(&self) -> bool {
        self.segments.iter().any(|s| s.speaker_id.is_some())
    }

    /// 派生纯文本(不含时间戳/说话人前缀),用于喂给 LLM 的降级路径。
    pub fn plain_text(&self) -> String {
        self.segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiarizeInfo {
    pub model: String,
    pub backend: Backend,
    pub mode: DiarizeMode,
    pub num_speakers_detected: u32,
    pub speakers: Vec<SpeakerStat>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpeakerStat {
    pub speaker_id: u32,
    pub talk_time_ms: u64,
    pub segment_count: u32,
}

/// 说话人数量策略。
///
/// `Fixed` 是最推荐的:它把开放集聚类搜索降级成封闭集固定 k 求解,
/// 见技术方案 §7.2。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiarizeMode {
    /// 自动检测人数(省事,长音频容易过分割)
    Auto,
    /// 指定人数(最准)
    Fixed(u8),
    /// 人数范围(折中)
    Range(u8, u8),
}

impl DiarizeMode {
    pub fn label(&self) -> String {
        match self {
            DiarizeMode::Auto => "自动检测".to_string(),
            DiarizeMode::Fixed(n) => format!("指定 {n} 人"),
            DiarizeMode::Range(a, b) => format!("{a}~{b} 人"),
        }
    }

    /// 传给 diarization 后端的显式聚类数。None 表示交给它自动判断。
    pub fn explicit_cluster_count(&self) -> Option<u8> {
        match self {
            DiarizeMode::Auto => None,
            DiarizeMode::Fixed(n) => Some(*n),
            DiarizeMode::Range(a, _) => Some(*a),
        }
    }

    /// 缓存键的稳定表示(枚举含 u8 载荷,需要能进哈希)。
    pub fn cache_tag(&self) -> String {
        match self {
            DiarizeMode::Auto => "auto".into(),
            DiarizeMode::Fixed(n) => format!("fixed{n}"),
            DiarizeMode::Range(a, b) => format!("range{a}-{b}"),
        }
    }
}

// ---------------------------------------------------------------------------
// 硬件后端
// ---------------------------------------------------------------------------

/// 计算后端。见技术方案 §3.1。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Cuda,
    Vulkan,
    Metal,
    Rocm,
    Sycl,
    OpenCl,
    Cpu,
}

impl Backend {
    /// 探测与自检的优先级顺序。CPU 永远在最后作为兜底。
    pub const PRIORITY: [Backend; 7] = [
        Backend::Cuda,
        Backend::Vulkan,
        Backend::Metal,
        Backend::Rocm,
        Backend::Sycl,
        Backend::OpenCl,
        Backend::Cpu,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Cuda => "cuda",
            Backend::Vulkan => "vulkan",
            Backend::Metal => "metal",
            Backend::Rocm => "rocm",
            Backend::Sycl => "sycl",
            Backend::OpenCl => "opencl",
            Backend::Cpu => "cpu",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Backend::Cuda => "CUDA",
            Backend::Vulkan => "Vulkan",
            Backend::Metal => "Metal",
            Backend::Rocm => "ROCm",
            Backend::Sycl => "SYCL",
            Backend::OpenCl => "OpenCL",
            Backend::Cpu => "CPU",
        }
    }

    pub fn is_gpu(&self) -> bool {
        !matches!(self, Backend::Cpu)
    }

    /// whisper.cpp sidecar 子目录名。sidecar 架构下"换后端"就是换一个 exe 路径。
    pub fn sidecar_dir(&self) -> &'static str {
        match self {
            Backend::Cpu => "cpu",
            Backend::Cuda => "cuda",
            Backend::Vulkan => "vulkan",
            Backend::Metal => "metal",
            Backend::Rocm => "rocm",
            Backend::Sycl => "sycl",
            Backend::OpenCl => "opencl",
        }
    }

    pub fn parse(s: &str) -> Option<Backend> {
        match s.to_ascii_lowercase().as_str() {
            "cuda" => Some(Backend::Cuda),
            "vulkan" => Some(Backend::Vulkan),
            "metal" => Some(Backend::Metal),
            "rocm" | "hip" => Some(Backend::Rocm),
            "sycl" => Some(Backend::Sycl),
            "opencl" | "cl" => Some(Backend::OpenCl),
            "cpu" => Some(Backend::Cpu),
            _ => None,
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 用户的后端偏好。`Auto` 走探测+自检+降级链,`Force` 失败时明确报错而不静默降级。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendPref {
    Auto,
    Force(Backend),
}

impl Default for BackendPref {
    fn default() -> Self {
        BackendPref::Auto
    }
}

// ---------------------------------------------------------------------------
// 模型档位
// ---------------------------------------------------------------------------

/// 转写模型档位。探测到的硬件决定推荐档位(见技术方案 §3.3)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    Tiny,
    Base,
    Small,
    Medium,
    LargeV3Turbo,
}

impl ModelTier {
    pub fn file_name(&self) -> &'static str {
        match self {
            ModelTier::Tiny => "ggml-tiny.bin",
            ModelTier::Base => "ggml-base.bin",
            ModelTier::Small => "ggml-small.bin",
            ModelTier::Medium => "ggml-medium.bin",
            ModelTier::LargeV3Turbo => "ggml-large-v3-turbo.bin",
        }
    }

    /// 近似文件大小(MB),用于下载进度与空间检查。
    pub fn approx_mb(&self) -> u64 {
        match self {
            ModelTier::Tiny => 75,
            ModelTier::Base => 142,
            ModelTier::Small => 466,
            ModelTier::Medium => 1500,
            ModelTier::LargeV3Turbo => 1620,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ModelTier::Tiny => "tiny",
            ModelTier::Base => "base",
            ModelTier::Small => "small",
            ModelTier::Medium => "medium",
            ModelTier::LargeV3Turbo => "large-v3-turbo",
        }
    }

    pub fn parse(s: &str) -> Option<ModelTier> {
        match s.to_ascii_lowercase().as_str() {
            "tiny" => Some(ModelTier::Tiny),
            "base" => Some(ModelTier::Base),
            "small" => Some(ModelTier::Small),
            "medium" => Some(ModelTier::Medium),
            "large-v3-turbo" | "large_v3_turbo" | "turbo" | "large" => {
                Some(ModelTier::LargeV3Turbo)
            }
            _ => None,
        }
    }
}

impl std::fmt::Display for ModelTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

// ---------------------------------------------------------------------------
// 说话人标签
// ---------------------------------------------------------------------------

/// 会话内的说话人标签(展示层)。改名只动这里,不动转写本身。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SpeakerLabels {
    pub session_id: String,
    /// 每次改名递增,用于判断已生成的总结是否过时。
    pub labels_version: u32,
    pub labels: BTreeMap<u32, SpeakerLabel>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpeakerLabel {
    pub display_name: String,
    pub color: String,
    /// 关联到的跨会话音色档案(若有)。声纹向量本身永不出本机。
    pub profile_id: Option<String>,
}

/// 预设调色板 —— 区分度高且对色盲友好。按 speaker_id 取模分配。
pub const SPEAKER_PALETTE: [&str; 10] = [
    "#4A90D9", "#D97A4A", "#4CAF50", "#9C6ADE", "#E5B33A",
    "#3AB8B8", "#D95A8A", "#7A8B99", "#8BC34A", "#B5651D",
];

impl SpeakerLabels {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            labels_version: 0,
            labels: BTreeMap::new(),
        }
    }

    /// 确保每个出现过的 speaker_id 都有标签,缺省名为"说话人 N"。
    pub fn ensure(&mut self, speaker_ids: impl IntoIterator<Item = u32>) {
        for id in speaker_ids {
            self.labels.entry(id).or_insert_with(|| SpeakerLabel {
                display_name: format!("说话人 {}", id + 1),
                color: SPEAKER_PALETTE[(id as usize) % SPEAKER_PALETTE.len()].to_string(),
                profile_id: None,
            });
        }
    }

    pub fn display_name(&self, speaker_id: u32) -> String {
        self.labels
            .get(&speaker_id)
            .map(|l| l.display_name.clone())
            .unwrap_or_else(|| format!("说话人 {}", speaker_id + 1))
    }

    /// 改名。返回是否真的发生了变化。
    pub fn rename(&mut self, speaker_id: u32, name: impl Into<String>) -> bool {
        let name = name.into();
        match self.labels.get_mut(&speaker_id) {
            Some(l) if l.display_name != name => {
                l.display_name = name;
                self.labels_version += 1;
                true
            }
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// 场景与总结
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scene {
    Lecture,
    Meeting,
    Interview,
    Other,
}

impl Scene {
    pub fn label(&self) -> &'static str {
        match self {
            Scene::Lecture => "课堂",
            Scene::Meeting => "会议",
            Scene::Interview => "访谈",
            Scene::Other => "其他",
        }
    }

    pub fn parse(s: &str) -> Option<Scene> {
        match s.to_ascii_lowercase().as_str() {
            "lecture" | "class" | "course" => Some(Scene::Lecture),
            "meeting" | "conference" => Some(Scene::Meeting),
            "interview" => Some(Scene::Interview),
            "other" => Some(Scene::Other),
            _ => None,
        }
    }
}

/// 第 1 段:场景判断结果。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SceneVerdict {
    pub scene: Scene,
    pub confidence: f32,
    pub evidence: String,
    pub suggested_focus: Vec<String>,
}

impl SceneVerdict {
    /// 置信度低于阈值时 UI 应显式提示"不确定"并给出一键切换。
    pub const LOW_CONFIDENCE: f32 = 0.6;

    pub fn is_low_confidence(&self) -> bool {
        self.confidence < Self::LOW_CONFIDENCE
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    /// 缓存命中的输入 token(DeepSeek 有折扣)
    pub cached_input: u64,
}

impl TokenUsage {
    pub fn merge(&mut self, other: &TokenUsage) {
        self.input += other.input;
        self.output += other.output;
        self.cached_input += other.cached_input;
    }
}

/// 第 2 段:总结结果。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub content_md: String,
    pub scene: Scene,
    pub model: String,
    pub usage: TokenUsage,
    /// 生成时所用的标签版本,用于判断改名后是否已过时。
    pub labels_version: u32,
    /// 是否因为长音频而走了 map-reduce
    pub used_map_reduce: bool,
}

impl Summary {
    /// 改名之后,已生成的总结就过时了 —— UI 应提示重新生成。
    pub fn is_stale(&self, labels: &SpeakerLabels) -> bool {
        self.labels_version < labels.labels_version
    }
}

// ---------------------------------------------------------------------------
// 思维导图
// ---------------------------------------------------------------------------

/// 思维导图。
///
/// 用 **Mermaid 文本**而不是图片,理由:
/// - LLM 生成结构化文本比生成图片可靠得多(后者要另一条图像模型链路)
/// - 文本可版本控制、可手改、可 diff
/// - 界面里能渲染成可缩放的矢量图
///
/// **但 LLM 生成的 Mermaid 经常有小语法错**(括号不匹配、节点名含特殊字符),
/// 所以始终保留一份文本大纲作为降级产物 —— 图渲染失败时用户仍然有东西看。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MindMap {
    /// Mermaid 源码(以 `mindmap` 开头)
    pub mermaid: String,
    /// 纯文本层级大纲。既作降级,也便于 grep。
    #[serde(default)]
    pub outline: String,
    /// 语法自检是否通过(仅做基础检查,真正的渲染由前端负责)
    #[serde(default)]
    pub syntax_ok: bool,
    /// 自检发现的问题
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub syntax_issues: Vec<String>,
}

impl MindMap {
    /// 从模型输出构造。**会自动修掉能修的语法错误。**
    ///
    /// 实测 LLM 最常见的错误是漏掉形状节点的 ID(`((文本))` 应为 `id((文本))`),
    /// 补一个不显示的 `n1` 就行 —— 与其报错让用户看到一张渲染失败的图,
    /// 不如先修再报。
    pub fn new(mermaid: impl Into<String>) -> Self {
        let raw = mermaid.into();
        // 先自检:把原始问题记录下来,便于排查提示词质量
        let (_, issues_before) = check_mermaid(&raw);

        let repaired = repair_mermaid(&raw);
        let (syntax_ok, issues_after) = check_mermaid(&repaired);

        // 报给用户的是"修完之后还剩什么问题",同时保留修之前的记录
        let mut issues = issues_after;
        if !issues_before.is_empty() && issues.is_empty() {
            issues = vec![format!(
                "已自动修复 {} 处语法问题(原始:{})",
                issues_before.len(),
                issues_before.join("、")
            )];
        }

        Self {
            mermaid: repaired,
            outline: String::new(),
            // 修完能过就算通过 —— 前端渲染的是修复后的版本
            syntax_ok,
            syntax_issues: issues,
        }
    }

    pub fn with_outline(mut self, outline: impl Into<String>) -> Self {
        self.outline = outline.into();
        self
    }

    pub fn is_empty(&self) -> bool {
        self.mermaid.trim().is_empty() && self.outline.trim().is_empty()
    }
}

/// 对 LLM 生成的 Mermaid 做**基础**语法自检。
///
/// 这不是完整解析器 —— 真正的渲染在前端。这里只抓最常见的几类错误,
/// 目的是能在写盘前就给用户一个"这张图可能渲染不出来"的信号。
///
/// 检查项:
/// 1. 非空且以 `mindmap` / `graph` / `flowchart` 开头
/// 2. 括号 / 方括号 / 圆括号成对
/// 3. 带形状的节点必须有 ID
pub fn check_mermaid(src: &str) -> (bool, Vec<String>) {
    let mut issues = Vec::new();
    let t = src.trim();

    if t.is_empty() {
        return (false, vec!["思维导图内容为空".into()]);
    }

    let first_line = t.lines().next().unwrap_or("").trim().to_ascii_lowercase();
    if !(first_line.starts_with("mindmap")
        || first_line.starts_with("graph")
        || first_line.starts_with("flowchart"))
    {
        issues.push(format!(
            "首行应为 mindmap / graph / flowchart,实际是「{}」",
            first_line.chars().take(30).collect::<String>()
        ));
    }

    // 括号配对(忽略引号内的内容,避免误判)
    for (open, close, name) in [('[', ']', "方括号"), ('(', ')', "圆括号"), ('{', '}', "花括号")] {
        let mut depth = 0i32;
        let mut in_quote = false;
        let mut min_depth = 0i32;
        for c in t.chars() {
            if c == '"' {
                in_quote = !in_quote;
                continue;
            }
            if in_quote {
                continue;
            }
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                min_depth = min_depth.min(depth);
            }
        }
        if depth != 0 || min_depth < 0 {
            issues.push(format!("{name}不匹配"));
        }
    }

    // 节点名里的裸引号会截断标签
    let quotes = t.matches('"').count();
    if quotes % 2 != 0 {
        issues.push("引号数量为奇数(可能有未闭合的标签)".into());
    }

    // ★ 带形状的节点必须有 ID。
    //
    // Mermaid 官方语法:`id((圆))` / `id[方]` / `id{六边形}`,
    // 而**无形状**的节点可以直接写文本。
    // 所以一行以 `((` / `[[` / `{{` 开头 = 缺 ID = 渲染失败。
    for (i, line) in t.lines().enumerate().skip(1) {
        let lt = line.trim();
        if lt.is_empty() || lt.starts_with("%%") {
            continue;
        }
        let bad = lt.starts_with("((")
            || lt.starts_with("[[")
            || lt.starts_with("{{")
            || lt.starts_with("))")
            || lt.starts_with("]]")
            || lt.starts_with("}}");
        if bad {
            issues.push(format!(
                "第 {} 行「{}」缺少节点 ID(带形状的节点必须写成 `id{}`)",
                i + 1,
                lt.chars().take(20).collect::<String>(),
                lt.chars().take(2).collect::<String>()
            ));
        }
    }

    (issues.is_empty(), issues)
}

/// 自动修掉 Mermaid 里最常见的结构性错误,让它至少能渲染。
///
/// 目前只做一件有把握的事:**给缺 ID 的形状节点补上 ID**。
///
/// 依据是官方语法 —— `((文本))` 必须写成 `id((文本))`,
/// 而实测 LLM 经常漏掉那个 `id`(它更习惯无形状的 `Root` 写法)。
/// 补一个位置无关的 `n1` / `n2` 即可,ID 不显示在图上。
pub fn repair_mermaid(src: &str) -> String {
    let mut out = String::new();
    let mut counter = 0usize;

    for (idx, line) in src.lines().enumerate() {
        if idx == 0 {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let lt = line.trim();
        let indent = &line[..line.len() - line.trim_start().len()];

        let needs_id = !lt.is_empty()
            && !lt.starts_with("%%")
            && (lt.starts_with("((")
                || lt.starts_with("[[")
                || lt.starts_with("{{")
                || lt.starts_with("))")
                || lt.starts_with("]]")
                || lt.starts_with("}}"));

        if needs_id {
            counter += 1;
            out.push_str(indent);
            out.push_str(&format!("n{counter}{lt}"));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

// ---------------------------------------------------------------------------
// 哈希辅助
// ---------------------------------------------------------------------------

/// 计算文件内容的 sha256,作为会话 ID 与缓存键前缀。
pub fn content_hash_of(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 内容哈希的前两位,用作子目录,避免单目录文件过多。
pub fn hash_shard(hash: &str) -> &str {
    &hash[..2.min(hash.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_priority_ends_with_cpu() {
        assert_eq!(*Backend::PRIORITY.last().unwrap(), Backend::Cpu);
        assert!(!Backend::Cpu.is_gpu());
        assert!(Backend::Cuda.is_gpu());
    }

    #[test]
    fn backend_roundtrip() {
        for b in Backend::PRIORITY {
            assert_eq!(Backend::parse(b.as_str()), Some(b));
        }
    }

    #[test]
    fn diarize_mode_cluster_count() {
        assert_eq!(DiarizeMode::Auto.explicit_cluster_count(), None);
        assert_eq!(DiarizeMode::Fixed(4).explicit_cluster_count(), Some(4));
        assert_eq!(DiarizeMode::Range(3, 6).explicit_cluster_count(), Some(3));
    }

    #[test]
    fn rename_bumps_version_and_only_on_change() {
        let mut labels = SpeakerLabels::new("s1");
        labels.ensure([0, 1]);
        assert_eq!(labels.display_name(0), "说话人 1");
        assert!(labels.rename(0, "张老师"));
        assert_eq!(labels.labels_version, 1);
        assert_eq!(labels.display_name(0), "张老师");
        // 改成同一个名字不应递增版本
        assert!(!labels.rename(0, "张老师"));
        assert_eq!(labels.labels_version, 1);
    }

    #[test]
    fn summary_staleness_tracks_label_version() {
        let mut labels = SpeakerLabels::new("s1");
        labels.ensure([0]);
        let mut summary = Summary {
            content_md: "x".into(),
            scene: Scene::Lecture,
            model: "m".into(),
            usage: TokenUsage::default(),
            labels_version: labels.labels_version,
            used_map_reduce: false,
        };
        assert!(!summary.is_stale(&labels));
        labels.rename(0, "张老师");
        assert!(summary.is_stale(&labels));
        // 重新生成后不再过时
        summary.labels_version = labels.labels_version;
        assert!(!summary.is_stale(&labels));
    }

    #[test]
    fn model_tier_parse_and_size() {
        assert_eq!(ModelTier::parse("large-v3-turbo"), Some(ModelTier::LargeV3Turbo));
        assert_eq!(ModelTier::parse("TURBO"), Some(ModelTier::LargeV3Turbo));
        assert!(ModelTier::LargeV3Turbo.approx_mb() > ModelTier::Small.approx_mb());
        assert_eq!(ModelTier::Small.file_name(), "ggml-small.bin");
    }

    #[test]
    fn segment_duration_never_underflows() {
        let s = Segment::new(500, 100, "bad");
        assert_eq!(s.duration_ms(), 0);
    }

    #[test]
    fn hash_shard_short_input() {
        assert_eq!(hash_shard("a"), "a");
        assert_eq!(hash_shard("abcdef"), "ab");
    }

    #[test]
    fn dual_track_speakers_are_reliable() {
        assert!(AudioSourceKind::DualTrack.speakers_are_reliable());
        assert!(!AudioSourceKind::File.speakers_are_reliable());
    }

    // --- 思维导图语法 ------------------------------------------------------

    #[test]
    fn check_mermaid_accepts_valid_source() {
        let ok = "mindmap\n  root((主题))\n    要点A\n    要点B";
        let (valid, issues) = check_mermaid(ok);
        assert!(valid, "{issues:?}");
    }

    #[test]
    fn check_mermaid_accepts_plain_nodes() {
        // 无形状的节点直接写文本是合法的
        let src = "mindmap\n  Root\n    A\n      B\n      C";
        let (valid, issues) = check_mermaid(src);
        assert!(valid, "{issues:?}");
    }

    #[test]
    fn check_mermaid_rejects_missing_id_on_shaped_node() {
        // ★ 回归测试:Mermaid 官方语法要求 `id((文本))`,缺 ID 会渲染失败。
        //   实测 LLM 经常漏掉(它更习惯 `Root` 那种无形状写法)。
        let bad = "mindmap\n  ((神经网络))\n    要点";
        let (valid, issues) = check_mermaid(bad);
        assert!(!valid, "缺 ID 应判为非法");
        assert!(
            issues.iter().any(|i| i.contains("缺少节点 ID")),
            "{issues:?}"
        );
    }

    #[test]
    fn check_mermaid_rejects_empty_and_bad_header() {
        assert!(!check_mermaid("").0);
        let (valid, issues) = check_mermaid("随便一段文字\n  内容");
        assert!(!valid);
        assert!(issues.iter().any(|i| i.contains("首行")), "{issues:?}");
    }

    #[test]
    fn check_mermaid_rejects_unbalanced_brackets() {
        let (valid, issues) = check_mermaid("mindmap\n  root((主题)\n    要点");
        assert!(!valid);
        assert!(issues.iter().any(|i| i.contains("不匹配")), "{issues:?}");
    }

    #[test]
    fn repair_mermaid_adds_missing_ids() {
        let bad = "mindmap\n  ((根主题))\n    要点\n  [[方节点]]";
        let fixed = repair_mermaid(bad);
        assert!(fixed.contains("n1((根主题))"), "{fixed}");
        assert!(fixed.contains("n2[[方节点]]"), "{fixed}");
        // 修完必须能通过自检
        let (ok, issues) = check_mermaid(&fixed);
        assert!(ok, "修复后仍不合法: {issues:?}");
    }

    #[test]
    fn repair_mermaid_preserves_valid_source() {
        let good = "mindmap\n  root((主题))\n    要点A\n    要点B";
        let fixed = repair_mermaid(good);
        assert_eq!(fixed, good, "合法源码不该被改动");
    }

    #[test]
    fn repair_mermaid_keeps_indentation() {
        let bad = "mindmap\n    ((根))\n        要点";
        let fixed = repair_mermaid(bad);
        assert!(fixed.contains("    n1((根))"), "缩进应保留: {fixed}");
        assert!(fixed.contains("        要点"), "子节点缩进应保留: {fixed}");
    }

    #[test]
    fn mindmap_new_auto_repairs_and_reports() {
        // ★ 用户看到的应该是"能渲染的图",而不是一张失败的红框
        let mm = MindMap::new("mindmap\n  ((主题))\n    要点");
        assert!(mm.mermaid.contains("n1((主题))"), "{}", mm.mermaid);
        assert!(mm.syntax_ok, "修复后应判为合法:{:?}", mm.syntax_issues);
        // 但要如实告知修过
        assert!(
            mm.syntax_issues.iter().any(|i| i.contains("自动修复")),
            "{:?}",
            mm.syntax_issues
        );
    }

    #[test]
    fn mindmap_new_on_already_valid_has_no_issues() {
        let mm = MindMap::new("mindmap\n  root((主题))\n    要点");
        assert!(mm.syntax_ok);
        assert!(mm.syntax_issues.is_empty(), "{:?}", mm.syntax_issues);
    }

    #[test]
    fn mindmap_empty_detection() {
        assert!(MindMap::default().is_empty());
        let mm = MindMap::new("mindmap\n  root((x))");
        assert!(!mm.is_empty());
    }

    #[test]
    fn mindmap_with_outline() {
        let mm = MindMap::new("mindmap\n  root((x))").with_outline("- x");
        assert_eq!(mm.outline, "- x");
        assert!(!mm.is_empty());
    }
}
