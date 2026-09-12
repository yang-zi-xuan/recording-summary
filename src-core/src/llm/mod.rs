//! 云端 LLM 客户端与总结。见技术方案 §10。
//!
//! 三个关键设计:
//!
//! 1. **按 OpenAI 兼容协议实现,`base_url` 可配置** —— 只多一个输入框,
//!    换来通义/Kimi/硅基流动/Ollama/私有部署全部可用。反过来(先做死 DeepSeek
//!    以后再抽)成本高得多,因为 base_url 会散落各处。
//! 2. **场景判断分两段** —— 判断错了整份纪要结构就错了,而用户不知道怪谁。
//! 3. **prompt 稳定内容前置** —— DeepSeek 有上下文缓存折扣,把系统提示+模板+术语表
//!    放最前面、转写正文放后面,让前缀缓存能命中。零成本优化。

pub mod correct;
pub mod cost;
pub mod keyring;
pub mod prompt;

use crate::types::{MindMap, Scene, SceneVerdict, SpeakerLabels, Summary, TokenUsage, Transcript};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub use cost::PriceTable;
pub use keyring::{delete_api_key, load_api_key, store_api_key, KEYRING_SERVICE};

/// LLM 配置。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmConfig {
    /// 供应商标识(仅用于展示与预设)
    pub provider: String,
    /// ★ 可配置端点 —— 这是"不绑死一家"的关键
    pub base_url: String,
    pub model: String,
    /// 不落盘,运行时从凭据管理器或环境变量取
    #[serde(skip)]
    pub api_key: Option<String>,
    pub temperature: f32,
    pub timeout_secs: u64,
    /// 单次请求最大输出 token
    pub max_tokens: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: "DeepSeek".into(),
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-chat".into(),
            api_key: None,
            temperature: 0.3,
            timeout_secs: 300,
            max_tokens: 4096,
        }
    }
}

impl LlmConfig {
    /// 内置预设。**不做实时 /models 查询** —— API 挂了或没网时界面会空。
    pub fn presets() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
        vec![
            (
                "DeepSeek",
                "https://api.deepseek.com",
                vec!["deepseek-chat", "deepseek-reasoner"],
            ),
            (
                "通义千问",
                "https://dashscope.aliyuncs.com/compatible-mode/v1",
                vec!["qwen-plus", "qwen-max", "qwen-turbo"],
            ),
            (
                "Kimi",
                "https://api.moonshot.cn/v1",
                vec!["moonshot-v1-8k", "moonshot-v1-32k", "moonshot-v1-128k"],
            ),
            (
                "硅基流动",
                "https://api.siliconflow.cn/v1",
                vec!["deepseek-ai/DeepSeek-V3"],
            ),
            (
                "Ollama(本地)",
                "http://localhost:11434/v1",
                vec!["qwen2.5:14b", "llama3.1:8b"],
            ),
        ]
    }

    pub fn chat_endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    /// 模型选择提示 —— 用户通常不知道 chat 和 reasoner 该选哪个。
    pub fn model_hint(&self) -> &'static str {
        match self.model.as_str() {
            "deepseek-chat" => "通用,总结首选",
            "deepseek-reasoner" => "复杂推理,更慢更贵",
            _ => "自定义模型",
        }
    }

    /// 校验配置是否完整。
    pub fn validate(&self) -> Result<()> {
        if self.api_key.as_deref().unwrap_or("").trim().is_empty() {
            return Err(anyhow!(
                "未配置 API Key。请运行 `rs config set-key <KEY>` 或在设置中填写。"
            ));
        }
        if self.base_url.trim().is_empty() {
            return Err(anyhow!("Base URL 为空"));
        }
        if self.model.trim().is_empty() {
            return Err(anyhow!("未选择模型"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 错误映射(必须给可行动提示)
// ---------------------------------------------------------------------------

/// 把 HTTP 状态码映射成用户能看懂、能行动的提示。
///
/// 见技术方案 §10.5 —— 最后一条尤其重要:模型名会变(旧模型下线),
/// 用户手填了失效名字时必须能看懂原因。
pub fn explain_http_error(status: u16, body: &str) -> String {
    let snippet: String = body.chars().take(300).collect();
    match status {
        401 => "API Key 无效或已过期,请重新填写。".into(),
        402 => "账户余额不足,请充值。".into(),
        403 => "无权访问该模型,请检查账号权限或模型名。".into(),
        404 => format!("接口或模型不存在,请检查 Base URL 与模型名。\n{snippet}"),
        422 => format!("请求参数有误(常见于模型名已下线)。\n{snippet}"),
        429 => "请求过于频繁,已自动重试。若持续出现请降低并发。".into(),
        500..=599 => format!("服务端错误({status}),稍后重试。\n{snippet}"),
        _ => format!("请求失败(HTTP {status})。\n{snippet}"),
    }
}

// ---------------------------------------------------------------------------
// 客户端
// ---------------------------------------------------------------------------

/// OpenAI 兼容的聊天客户端。
pub struct LlmClient {
    cfg: LlmConfig,
    http: reqwest::Client,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    stream: bool,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    message: Option<RespMessage>,
}

#[derive(Deserialize)]
struct RespMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_cache_hit_tokens: u64,
}

/// 一次 chat 调用的结果。
#[derive(Clone, Debug)]
pub struct ChatOutcome {
    pub content: String,
    pub usage: TokenUsage,
}

impl LlmClient {
    pub fn new(cfg: LlmConfig) -> Result<Self> {        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .build()
            .context("构建 HTTP 客户端失败")?;
        Ok(Self { cfg, http })
    }

    pub fn config(&self) -> &LlmConfig {
        &self.cfg
    }

    /// 发一次 chat 请求,带 429/5xx 的退避重试。
    /// 同步版 chat。
    ///
    /// ⚠️ **只在确认调用方没有 runtime 时用。** 管线里更安全的做法是走
    /// [`BlockingSummarizer::correct_blocking`] —— 它复用已经建好的那个
    /// runtime,不会嵌套。
    pub fn chat_blocking(&self, system: &str, user: &str) -> Result<ChatOutcome> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow!("创建 runtime 失败: {e}"))?;
        rt.block_on(self.chat(system, user))
    }

    pub async fn chat(&self, system: &str, user: &str) -> Result<ChatOutcome> {
        self.cfg.validate()?;
        let key = self.cfg.api_key.clone().unwrap_or_default();

        let body = ChatRequest {
            model: &self.cfg.model,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: system,
                },
                ChatMessage {
                    role: "user",
                    content: user,
                },
            ],
            temperature: self.cfg.temperature,
            max_tokens: self.cfg.max_tokens,
            response_format: None,
            stream: false,
        };

        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                let backoff = Duration::from_millis(800 * 2u64.pow(attempt - 1));
                tracing::warn!("请求失败,{backoff:?} 后重试(第 {attempt}/2 次)");
                tokio::time::sleep(backoff).await;
            }

            let resp = self
                .http
                .post(self.cfg.chat_endpoint())
                .bearer_auth(&key)
                .json(&body)
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    // 网络层错误:提示检查代理(见 §10.5)
                    let hint = if e.is_timeout() {
                        "请求超时。若在国内访问境外服务,请检查代理设置。"
                    } else {
                        "网络不通。请检查网络或代理设置。"
                    };
                    last_err = Some(anyhow!("{hint}\n底层错误: {e}"));
                    continue;
                }
            };

            let status = resp.status();
            if status.is_success() {
                let parsed: ChatResponse = resp
                    .json()
                    .await
                    .context("解析 LLM 响应失败(响应不是合法 JSON)")?;
                let content = parsed
                    .choices
                    .into_iter()
                    .find_map(|c| c.message.and_then(|m| m.content))
                    .ok_or_else(|| anyhow!("LLM 返回了空内容"))?;
                let usage = parsed
                    .usage
                    .map(|u| TokenUsage {
                        input: u.prompt_tokens,
                        output: u.completion_tokens,
                        cached_input: u.prompt_cache_hit_tokens,
                    })
                    .unwrap_or_default();
                return Ok(ChatOutcome { content, usage });
            }

            let code = status.as_u16();
            let text = resp.text().await.unwrap_or_default();
            let msg = explain_http_error(code, &text);

            // 只有 429 / 5xx 值得重试;4xx 重试没意义
            if code == 429 || code >= 500 {
                last_err = Some(anyhow!(msg));
                continue;
            }
            return Err(anyhow!(msg));
        }

        Err(last_err.unwrap_or_else(|| anyhow!("请求失败,已重试 2 次")))
    }

    /// 连通性测试(给设置页的"测试连接"按钮用)。
    pub async fn test_connection(&self) -> Result<String> {
        let out = self.chat("你是一个测试助手。", "回复两个字:正常").await?;
        Ok(out.content.trim().to_string())
    }
}

// ---------------------------------------------------------------------------
// 总结器
// ---------------------------------------------------------------------------

pub trait Summarizer: Send + Sync {
    fn detect_scene(&self, transcript: &Transcript) -> Result<SceneVerdict>;

    /// 转写纠错(同音字与术语)。见 [`correct`] 模块。
    ///
    /// **有默认实现:什么也不做。** 这样 [`MockSummarizer`] 之类的实现
    /// 不必都去写一遍 —— 而管线里靠"纠错前后文本是否有变化"来判断
    /// 这一步是否真的生效,不依赖 `Option`。
    ///
    /// 真实实现会覆盖它。
    fn correct_blocking(
        &self,
        _segments: &mut [crate::types::Segment],
        _terms: &[String],
    ) -> (correct::CorrectionStats, Vec<String>) {
        (correct::CorrectionStats::default(), Vec::new())
    }

    /// 详细总结:完整结构(知识点 / 例题 / 待办 / 复习提纲)。
    fn summarize(
        &self,
        transcript: &Transcript,
        scene: Scene,
        labels: &SpeakerLabels,
        labels_version: u32,
    ) -> Result<Summary>;

    /// 简略总结:一页速览。**不是详细版的压缩,而是换了目标**(见 prompt::brief_structure)。
    ///
    /// 有默认实现:退回详细总结。这样 MockSummarizer 之类不必都实现它,
    /// 而真实实现会覆盖。
    fn summarize_brief(
        &self,
        transcript: &Transcript,
        scene: Scene,
        labels: &SpeakerLabels,
        labels_version: u32,
    ) -> Result<Summary> {
        self.summarize(transcript, scene, labels, labels_version)
    }

    /// 思维导图(Mermaid)。
    ///
    /// 默认返回空 —— 调用方应把"空导图"当成"这个总结器不支持",而不是错误。
    fn summarize_mindmap(&self, _transcript: &Transcript, _scene: Scene) -> Result<MindMap> {
        Ok(MindMap::default())
    }
}

/// 同步包装 —— 内部用 tokio 运行时驱动异步客户端。
///
/// 这样 `Summarizer` trait 保持同步签名,CLI 和 GUI 都不必是 async。
///
/// **运行时跑在一个专用线程里**,而不是现建现 drop:
/// 如果调用方本身处在 tokio 异步上下文中(例如 `#[tokio::main]`),
/// 直接 drop 一个 runtime 会 panic —— "Cannot drop a runtime in a context
/// where blocking is not allowed"。放在专属线程里则与调用方无关。
pub struct BlockingSummarizer {
    client: LlmClient,
    runtime: Option<tokio::runtime::Runtime>,
    price: PriceTable,
    /// 超过此 token 估算值就走 map-reduce
    map_reduce_threshold_tokens: usize,
}

impl Drop for BlockingSummarizer {
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            // 在专属线程里销毁运行时,避开调用方的异步上下文
            let _ = std::thread::spawn(move || drop(rt)).join();
        }
    }
}

impl BlockingSummarizer {
    pub fn new(cfg: LlmConfig, price: PriceTable) -> Result<Self> {
        let (runtime, client) = std::thread::spawn(move || -> Result<_> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("创建 tokio 运行时失败")?;
            let client = LlmClient::new(cfg)?;
            Ok((rt, client))
        })
        .join()
        .map_err(|_| anyhow!("初始化 LLM 运行时失败"))??;

        Ok(Self {
            client,
            runtime: Some(runtime),
            price,
            map_reduce_threshold_tokens: 6000,
        })
    }

    fn rt(&self) -> &tokio::runtime::Runtime {
        self.runtime.as_ref().expect("runtime 已被释放")
    }

    pub fn price_table(&self) -> &PriceTable {
        &self.price
    }

    pub fn model(&self) -> String {
        self.client.config().model.clone()
    }

    /// 同步版的连通性测试(给设置页的"测试连接"按钮用)。
    pub fn test_connection_blocking(&self) -> Result<String> {
        self.rt().block_on(self.client.test_connection())
    }

    /// 只要转写文本带说话人前缀(纯文本视图),便于 LLM 引用"谁说了什么"。
    pub fn render_for_llm(&self, t: &Transcript, labels: &SpeakerLabels) -> String {
        let mut out = String::new();
        for u in crate::pipeline::view::merge_for_dialogue_default(&t.segments) {
            match u.speaker_id {
                Some(id) => out.push_str(&format!("{}:{}\n", labels.display_name(id), u.text)),
                None => out.push_str(&format!("{}\n", u.text)),
            }
        }
        out
    }
}

impl Summarizer for BlockingSummarizer {
    /// 转写纠错。
    ///
    /// ⚠️ **必须实现在这个 trait impl 里,不能只在 inherent impl 里。
    ///
    /// 我第一版把它写成了 `impl BlockingSummarizer` 下的固有方法,结果管线
    /// 里 `sum.correct_blocking(...)`(`sum` 是 `&dyn Summarizer`)调的是
    /// trait 的**默认空实现** —— 编译通过、测试通过、日志显示"纠错:未纠错",
    /// 而真正的纠错一行没跑。
    ///
    /// 这和本项目里反复出现的"死代码"是同一类:代码写了但走不到,
    /// 而且**没有任何报错**。
    fn correct_blocking(
        &self,
        segments: &mut [crate::types::Segment],
        terms: &[String],
    ) -> (correct::CorrectionStats, Vec<String>) {
        // 复用自己那个 runtime —— 在管线里新建会嵌套(会 panic)
        correct::correct_with(&self.client, self.rt(), segments, terms)
    }

    fn detect_scene(&self, transcript: &Transcript) -> Result<SceneVerdict> {
        let samples = prompt::scene_samples(transcript);
        let (system, user) = prompt::scene_detect_prompt(&samples);
        let out = self.rt().block_on(self.client.chat(&system, &user))?;
        prompt::parse_scene_verdict(&out.content)
    }

    fn summarize(
        &self,
        transcript: &Transcript,
        scene: Scene,
        labels: &SpeakerLabels,
        labels_version: u32,
    ) -> Result<Summary> {
        let rendered = self.render_for_llm(transcript, labels);
        let est = prompt::estimate_tokens(&rendered);
        let use_map_reduce = est > self.map_reduce_threshold_tokens;

        let mut usage = TokenUsage::default();
        let content_md = if use_map_reduce {
            // ① 分段摘要(在句子边界切,见技术方案 §9.3)
            let chunks = prompt::split_for_map_reduce(&rendered);
            let mut partials = Vec::new();
            for (i, c) in chunks.iter().enumerate() {
                let (s, u) = prompt::map_prompt(scene, c, i + 1, chunks.len());
                let out = self.rt().block_on(self.client.chat(&s, &u))?;
                usage.merge(&out.usage);
                partials.push(out.content);
            }
            // ② 聚合(聚合 prompt 也必须按场景分支)
            let (s, u) = prompt::reduce_prompt(scene, &partials);
            let out = self.rt().block_on(self.client.chat(&s, &u))?;
            usage.merge(&out.usage);
            out.content
        } else {
            let (s, u) = prompt::single_prompt(scene, &rendered);
            let out = self.rt().block_on(self.client.chat(&s, &u))?;
            usage.merge(&out.usage);
            out.content
        };

        Ok(Summary {
            content_md,
            scene,
            model: self.model(),
            usage,
            labels_version,
            used_map_reduce: use_map_reduce,
        })
    }

    fn summarize_brief(
        &self,
        transcript: &Transcript,
        scene: Scene,
        labels: &SpeakerLabels,
        labels_version: u32,
    ) -> Result<Summary> {
        let rendered = self.render_for_llm(transcript, labels);
        let est = prompt::estimate_tokens(&rendered);
        let use_map_reduce = est > self.map_reduce_threshold_tokens;

        let mut usage = TokenUsage::default();
        let content_md = if use_map_reduce {
            let chunks = prompt::split_for_map_reduce(&rendered);
            let mut partials = Vec::new();
            for (i, c) in chunks.iter().enumerate() {
                let (s, u) = prompt::map_prompt(scene, c, i + 1, chunks.len());
                let out = self.rt().block_on(self.client.chat(&s, &u))?;
                usage.merge(&out.usage);
                partials.push(out.content);
            }
            // 简略版有自己的聚合提示 —— 目标是"压成一页",不是"整理成纪要"
            let (s, u) = prompt::brief_reduce_prompt(scene, &partials);
            let out = self.rt().block_on(self.client.chat(&s, &u))?;
            usage.merge(&out.usage);
            out.content
        } else {
            let (s, u) = prompt::brief_prompt(scene, &rendered);
            let out = self.rt().block_on(self.client.chat(&s, &u))?;
            usage.merge(&out.usage);
            out.content
        };

        Ok(Summary {
            content_md,
            scene,
            model: self.model(),
            usage,
            labels_version,
            used_map_reduce: use_map_reduce,
        })
    }

    fn summarize_mindmap(&self, transcript: &Transcript, scene: Scene) -> Result<MindMap> {
        let rendered = self.render_for_llm(transcript, &SpeakerLabels::new("anon"));
        let (s, u) = prompt::mindmap_prompt(scene, &rendered);
        let out = self.rt().block_on(self.client.chat(&s, &u))?;

        // 模型常不听话地包代码块或加解释 —— 剥掉
        let mermaid = prompt::extract_mermaid(&out.content);
        let outline = prompt::mermaid_to_outline(&mermaid);
        Ok(MindMap::new(mermaid).with_outline(outline))
    }
}

// ---------------------------------------------------------------------------
// 假总结器(测试用,不依赖网络与 API Key)
// ---------------------------------------------------------------------------

/// 确定性假总结器。让端到端测试不依赖 API Key。
#[derive(Debug, Default)]
pub struct MockSummarizer {
    pub scene: Option<Scene>,
    pub fail_scene: bool,
}

impl MockSummarizer {
    pub fn new() -> Self {
        Self {
            scene: None,
            fail_scene: false,
        }
    }

    /// 简单地按关键词猜场景 —— 与真实 LLM 的分支行为一致,便于测试两条路径。
    pub fn guess_scene(text: &str) -> (Scene, f32) {
        let lecture = ["这节课", "作业", "考试", "同学", "公式", "例题"];
        let meeting = ["议题", "决议", "待办", "下周", "负责", "进度"];
        let lc = lecture.iter().filter(|k| text.contains(**k)).count();
        let mc = meeting.iter().filter(|k| text.contains(**k)).count();
        if lc == 0 && mc == 0 {
            (Scene::Other, 0.3)
        } else if lc >= mc {
            (Scene::Lecture, 0.7 + 0.05 * lc as f32)
        } else {
            (Scene::Meeting, 0.7 + 0.05 * mc as f32)
        }
    }
}

impl Summarizer for MockSummarizer {
    fn detect_scene(&self, transcript: &Transcript) -> Result<SceneVerdict> {
        if self.fail_scene {
            return Err(anyhow!("(mock)场景判断失败"));
        }
        let (scene, conf) = self.scene.map(|s| (s, 0.95)).unwrap_or_else(|| {
            MockSummarizer::guess_scene(&transcript.raw_text)
        });
        Ok(SceneVerdict {
            scene,
            confidence: conf,
            evidence: "(mock)基于关键词判断".into(),
            suggested_focus: vec!["要点".into()],
        })
    }

    fn summarize(
        &self,
        transcript: &Transcript,
        scene: Scene,
        labels: &SpeakerLabels,
        labels_version: u32,
    ) -> Result<Summary> {
        let mut md = format!("# (mock){}纪要\n\n", scene.label());
        md.push_str(&format!("- 段落数:{}\n", transcript.segments.len()));
        md.push_str(&format!("- 时长:{} 秒\n", transcript.duration_ms / 1000));
        let speakers: Vec<String> = labels
            .labels
            .keys()
            .map(|k| labels.display_name(*k))
            .collect();
        if !speakers.is_empty() {
            md.push_str(&format!("- 发言人:{}\n", speakers.join("、")));
        }
        Ok(Summary {
            content_md: md,
            scene,
            model: "mock".into(),
            usage: TokenUsage {
                input: 100,
                output: 50,
                cached_input: 80,
            },
            labels_version,
            used_map_reduce: false,
        })
    }

    /// 假简略总结:结构更短,便于端到端测试区分两份产物。
    fn summarize_brief(
        &self,
        transcript: &Transcript,
        scene: Scene,
        labels: &SpeakerLabels,
        labels_version: u32,
    ) -> Result<Summary> {
        let mut md = format!("# (mock){}速览\n\n", scene.label());
        md.push_str("## 一句话\n(mock)这是一份简略总结。\n\n");
        md.push_str("## 核心要点\n");
        for i in 0..3.min(transcript.segments.len().max(1)) {
            md.push_str(&format!("- 要点 {}\n", i + 1));
        }
        let speakers: Vec<String> = labels
            .labels
            .keys()
            .map(|k| labels.display_name(*k))
            .collect();
        if !speakers.is_empty() {
            md.push_str(&format!("\n## 参与人\n{}\n", speakers.join("、")));
        }
        Ok(Summary {
            content_md: md,
            scene,
            model: "mock".into(),
            usage: TokenUsage {
                input: 80,
                output: 30,
                cached_input: 60,
            },
            labels_version,
            used_map_reduce: false,
        })
    }

    /// 假思维导图:生成一份语法正确的 Mermaid,便于前端渲染测试。
    fn summarize_mindmap(&self, transcript: &Transcript, scene: Scene) -> Result<MindMap> {
        let mut mm = String::from("mindmap\n");
        mm.push_str(&format!("  root(({}))\n", scene.label()));
        let n = transcript.segments.len().min(4).max(1);
        for i in 0..n {
            mm.push_str(&format!("    模块{}\n", i + 1));
            mm.push_str(&format!("      要点{}\n", i + 1));
        }
        let outline = prompt::mermaid_to_outline(&mm);
        Ok(MindMap::new(mm).with_outline(outline))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Segment;

    fn t(text: &str) -> Transcript {
        let segs = vec![Segment::new(0, 1000, text)];
        Transcript {
            engine: "mock".into(),
            model: "m".into(),
            backend: crate::types::Backend::Cpu,
            backend_diarize: None,
            language: Some("zh".into()),
            raw_text: text.into(),
            segments: segs,
            duration_ms: 1000,
            diarize: None,
        }
    }

    #[test]
    fn default_config_points_at_deepseek() {
        let c = LlmConfig::default();
        assert_eq!(c.model, "deepseek-chat");
        assert_eq!(c.chat_endpoint(), "https://api.deepseek.com/chat/completions");
    }

    #[test]
    fn endpoint_handles_trailing_slash() {
        let mut c = LlmConfig::default();
        c.base_url = "https://api.deepseek.com/".into();
        assert_eq!(c.chat_endpoint(), "https://api.deepseek.com/chat/completions");
    }

    #[test]
    fn base_url_is_configurable_for_other_providers() {
        // ★ 这是"不绑死一家"的核心:换 base_url 就能接别家
        let mut c = LlmConfig::default();
        for (_, url, models) in LlmConfig::presets() {
            c.base_url = url.to_string();
            c.model = models[0].to_string();
            assert!(!c.chat_endpoint().is_empty());
            assert!(c.chat_endpoint().ends_with("/chat/completions"));
        }
    }

    #[test]
    fn validate_requires_api_key() {
        let c = LlmConfig::default();
        assert!(c.validate().is_err());
        let mut c = c;
        c.api_key = Some("  ".into());
        assert!(c.validate().is_err(), "空白 key 应视为未配置");
        c.api_key = Some("sk-x".into());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_model() {
        let mut c = LlmConfig::default();
        c.api_key = Some("sk-x".into());
        c.model = "".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn http_errors_map_to_actionable_text() {
        assert!(explain_http_error(401, "").contains("无效或已过期"));
        assert!(explain_http_error(402, "").contains("余额不足"));
        assert!(explain_http_error(429, "").contains("频繁"));
        assert!(explain_http_error(500, "boom").contains("服务端"));
        // 模型下线是最需要说清楚的一种
        let m = explain_http_error(422, "model not found");
        assert!(m.contains("模型名已下线") || m.contains("model not found"));
    }

    #[test]
    fn presets_include_multiple_providers() {
        let p = LlmConfig::presets();
        assert!(p.len() >= 4, "应内置多家预设");
        assert!(p.iter().any(|(n, _, _)| *n == "DeepSeek"));
        // Ollama 说明本地部署也走同一条路
        assert!(p.iter().any(|(_, u, _)| u.contains("localhost")));
    }

    #[test]
    fn model_hint_explains_choice() {
        let mut c = LlmConfig::default();
        assert!(c.model_hint().contains("总结首选"));
        c.model = "deepseek-reasoner".into();
        assert!(c.model_hint().contains("更慢更贵"));
    }

    #[test]
    fn mock_summarizer_detects_lecture() {
        let m = MockSummarizer::new();
        let v = m
            .detect_scene(&t("同学们,这节课讲第三章,作业是习题 3.1"))
            .unwrap();
        assert_eq!(v.scene, Scene::Lecture);
        assert!(!v.is_low_confidence());
    }

    #[test]
    fn mock_summarizer_detects_meeting() {
        let m = MockSummarizer::new();
        let v = m
            .detect_scene(&t("第一个议题是排期,决议如下,待办由张老师负责,下周交付"))
            .unwrap();
        assert_eq!(v.scene, Scene::Meeting);
    }

    #[test]
    fn low_confidence_is_flagged() {
        let m = MockSummarizer::new();
        let v = m.detect_scene(&t("嗯,那个,随便说两句")).unwrap();
        assert_eq!(v.scene, Scene::Other);
        assert!(v.is_low_confidence(), "置信度低时必须能被 UI 识别出来");
    }

    #[test]
    fn mock_summary_records_labels_version() {
        let m = MockSummarizer::new();
        let mut labels = SpeakerLabels::new("s");
        labels.ensure([0]);
        labels.rename(0, "张老师");
        let s = m.summarize(&t("内容"), Scene::Lecture, &labels, labels.labels_version).unwrap();
        assert_eq!(s.labels_version, 1);
        assert!(s.content_md.contains("张老师"), "总结里应出现真实姓名");
        // 之后改名 → 总结应被判为过时
        labels.rename(0, "李老师");
        assert!(s.is_stale(&labels));
    }

    #[test]
    fn mock_scene_failure_propagates() {
        let mut m = MockSummarizer::new();
        m.fail_scene = true;
        assert!(m.detect_scene(&t("x")).is_err());
    }

    #[test]
    fn render_for_llm_prefixes_speaker_names() {
        let mut labels = SpeakerLabels::new("s");
        labels.ensure([0, 1]);
        labels.rename(0, "张老师");
        labels.rename(1, "李同学");

        let mut tr = t("今天讲神经网络");
        tr.segments = vec![
            Segment::new(0, 1000, "今天讲神经网络"),
            Segment::new(1200, 2000, "老师这里不懂"),
        ];
        tr.segments[0].speaker_id = Some(0);
        tr.segments[1].speaker_id = Some(1);

        let sum = BlockingSummarizer::dummy_for_render();
        let out = sum.render_for_llm(&tr, &labels);
        assert!(out.contains("张老师:"), "{out}");
        assert!(out.contains("李同学:"), "{out}");
    }

    impl BlockingSummarizer {
        /// 只用于测试 `render_for_llm`(不需要网络)。
        fn dummy_for_render() -> Self {
            let mut cfg = LlmConfig::default();
            cfg.api_key = Some("x".into());
            Self::new(cfg, PriceTable::default()).unwrap()
        }
    }
}
