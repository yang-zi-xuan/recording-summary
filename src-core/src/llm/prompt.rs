//! Prompt 组装与解析。见技术方案 §9 与 §10.6。
//!
//! 两个设计要点:
//!
//! 1. **场景判断只采样,不喂全文** —— 但必须采样开头 + 中段 + 末尾。
//!    只看开头有真实风险:很多会议开头是闲聊、很多课程开头是点名,开头样本会误导判断。
//! 2. **稳定内容前置** —— 系统提示 + 模板 + 术语表放最前面,转写正文放后面,
//!    让 DeepSeek 的前缀缓存能命中。这是零成本优化,但一开始写反了后面要动所有 prompt。

use crate::types::{Scene, SceneVerdict, Transcript};

/// 粗估 token 数。
///
/// 不用引入 tokenizer:决策只需要一个量级判断(是否超过 map-reduce 阈值)。
/// 中文大致 1 字 ≈ 1 token,ASCII 大致 4 字符 ≈ 1 token。
pub fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if ('\u{4E00}'..='\u{9FFF}').contains(&ch)
            || ('\u{3000}'..='\u{303F}').contains(&ch)
            || ('\u{FF00}'..='\u{FFEF}').contains(&ch)
        {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

// ---------------------------------------------------------------------------
// 场景判断
// ---------------------------------------------------------------------------

/// 从转写中采样用于场景判断的片段:开头 + 中段 + 末尾。
pub fn scene_samples(t: &Transcript) -> String {
    let full = crate::pipeline::view::merge_for_dialogue_default(&t.segments)
        .into_iter()
        .map(|u| u.text)
        .collect::<Vec<_>>()
        .join("\n");

    let chars: Vec<char> = full.chars().collect();
    let n = chars.len();
    let take = 700usize;

    if n <= take * 3 {
        return full;
    }

    let head: String = chars[..take].iter().collect();
    let mid_start = n / 2 - take / 2;
    let mid: String = chars[mid_start..mid_start + take].iter().collect();
    let tail: String = chars[n - take..].iter().collect();

    format!("【开头】\n{head}\n\n【中段】\n{mid}\n\n【末尾】\n{tail}")
}

pub fn scene_detect_prompt(samples: &str) -> (String, String) {
    let system = "你是一个录音场景分类器。\
        只输出一个 JSON 对象,不要输出任何其他文字、不要用 markdown 代码块。"
        .to_string();

    let user = format!(
        r#"判断下面这段录音转写属于哪种场景。

可选场景:
- "lecture"   课堂/讲座:单人长时间讲解知识,有作业、考试、例题、板书等用语
- "meeting"   会议:多人讨论工作,有议题、决议、待办、负责人、期限等用语
- "interview" 访谈:一问一答,围绕特定主题深入交流
- "other"     其他:无法归入以上任何一类

要求:
1. 三段采样分别是开头、中段、末尾。**不要只看开头** —— 会议开头常是闲聊,
   课堂开头常是点名,只看开头容易误判。
2. confidence 反映你的确信程度(0.0~1.0)。判断依据不足时给低分,不要硬猜。
3. evidence 用一句话说明判断依据。

输出格式(严格 JSON):
{{"scene":"lecture","confidence":0.87,"evidence":"...","suggested_focus":["知识点","例题"]}}

转写采样:
---
{samples}
---"#
    );
    (system, user)
}

/// 解析场景判断结果。LLM 有时会包 ```json 代码块或加解释文字,这里做容错。
pub fn parse_scene_verdict(raw: &str) -> anyhow::Result<SceneVerdict> {
    let json = extract_json_object(raw)
        .ok_or_else(|| anyhow::anyhow!("无法从模型输出中提取 JSON:\n{}", truncate(raw, 400)))?;

    #[derive(serde::Deserialize)]
    struct Raw {
        scene: String,
        #[serde(default)]
        confidence: Option<f32>,
        #[serde(default)]
        evidence: Option<String>,
        #[serde(default)]
        suggested_focus: Option<Vec<String>>,
    }

    let r: Raw = serde_json::from_str(&json)
        .map_err(|e| anyhow::anyhow!("场景判断 JSON 解析失败: {e}\n原文:{json}"))?;

    let scene = Scene::parse(&r.scene).unwrap_or(Scene::Other);
    let confidence = r.confidence.unwrap_or(0.5).clamp(0.0, 1.0);

    Ok(SceneVerdict {
        scene,
        confidence,
        evidence: r.evidence.unwrap_or_default(),
        suggested_focus: r.suggested_focus.unwrap_or_default(),
    })
}

/// 从可能夹带 markdown 围栏或解释文字的输出里抠出第一个 JSON 对象。
pub fn extract_json_object(raw: &str) -> Option<String> {
    let cleaned = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```JSON")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    if cleaned.starts_with('{') {
        if let Some(s) = balanced_object(cleaned) {
            return Some(s);
        }
    }
    // 退一步:在整段文本里找第一个平衡的 {...}
    let start = raw.find('{')?;
    balanced_object(&raw[start..])
}

/// 找到从第一个 `{` 开始、括号平衡的子串。
fn balanced_object(s: &str) -> Option<String> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, ch) in s.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ---------------------------------------------------------------------------
// 总结模板
// ---------------------------------------------------------------------------

/// 场景对应的结构要求。**这是分场景模板的核心** ——
/// 课堂和会议的纪要结构差异很大,不能共用一套。
pub fn scene_structure(scene: Scene) -> &'static str {
    match scene {
        Scene::Lecture => {
            "按以下结构输出 Markdown:\n\
             ## 一句话摘要\n\
             ## 知识点梳理\n(每个知识点:概念名 + 简明解释 + 出现时间戳)\n\
             ## 例题与解法\n(若有)\n\
             ## 作业与考试提示\n(若有;没有就写“未提及”)\n\
             ## 复习提纲\n(给学生复习用的要点清单)"
        }
        Scene::Meeting => {
            "按以下结构输出 Markdown:\n\
             ## 一句话摘要\n\
             ## 议题\n\
             ## 决议\n(明确达成的结论)\n\
             ## 待办事项\n(用表格:任务 | 负责人 | 截止时间;没提到的写“未指定”)\n\
             ## 分歧点\n(若有)\n\
             ## 未决问题"
        }
        Scene::Interview => {
            "按以下结构输出 Markdown:\n\
             ## 一句话摘要\n\
             ## 主题脉络\n\
             ## 核心观点\n\
             ## 关键问答\n\
             ## 金句\n(值得原样保留的话)"
        }
        Scene::Other => {
            "按以下结构输出 Markdown:\n\
             ## 一句话摘要\n\
             ## 主要内容\n\
             ## 关键信息\n\
             ## 待跟进事项\n(若有)"
        }
    }
}

const COMMON_RULES: &str = "通用要求:\n\
     1. 只依据转写内容,不要编造未提及的信息。\n\
     2. 关键结论尽量带上时间戳(格式 [HH:MM:SS]),便于回听原话。\n\
     3. 若转写里有明显的同音错字或术语错误,按上下文纠正后使用正确写法。\n\
     4. 转写可能不完整或有噪音导致的缺字,不要因此编造内容。\n\
     5. 直接输出 Markdown 正文,不要用代码块包裹。";

/// 把长转写切成 map-reduce 用的分段。
///
/// ★ **必须在句子边界切** —— 否则会把一句话劈成两半,两段摘要都不完整。
/// 这与 §3.4 的音频分块是两件事,但都遵循"不要切在句子中间"这一条。
pub fn split_for_map_reduce(text: &str) -> Vec<String> {
    /// 每段目标字符数。中文 1 字 ≈ 1 token,4000 字留足上下文余量。
    const TARGET_CHARS: usize = 4000;
    const MAX_CHUNKS: usize = 40;

    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return vec![];
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();

    for line in lines {
        // 单行就超长(极端情况)→ 先冲掉当前段
        if line.chars().count() > TARGET_CHARS {
            if !cur.is_empty() {
                chunks.push(std::mem::take(&mut cur));
            }
            chunks.push(line.to_string());
            continue;
        }

        let would_be = cur.chars().count() + line.chars().count() + 1;
        // 上一行已收尾成句 且 再加就超标 → 在此断开(句子边界)
        if would_be > TARGET_CHARS && ends_sentence(&cur) {
            chunks.push(std::mem::take(&mut cur));
        } else if would_be > TARGET_CHARS * 3 / 2 {
            // 硬上限:即使没到句末也不能无限增长
            chunks.push(std::mem::take(&mut cur));
        }

        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(line);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }

    // 段数过多时按比例合并,避免请求数爆炸
    if chunks.len() > MAX_CHUNKS {
        let per = chunks.len().div_ceil(MAX_CHUNKS);
        chunks = chunks
            .chunks(per)
            .map(|g| g.join("\n"))
            .collect();
    }

    chunks
}

fn ends_sentence(s: &str) -> bool {
    matches!(
        s.trim_end().chars().last(),
        Some('。') | Some('!') | Some('?') | Some('…') | Some('.') | Some(';')
    )
}

/// 单次总结(短音频)。
pub fn single_prompt(scene: Scene, transcript_text: &str) -> (String, String) {
    // ★ 稳定内容全部放在 system 里,变化的正文放 user —— 这样前缀缓存能命中
    let system = format!(
        "你是一个专业的录音纪要助手。你会把课堂或会议的转写整理成结构化纪要。\n\n{}\n\n{}",
        scene_structure(scene),
        COMMON_RULES    );
    let user = format!("以下是转写内容:\n\n{transcript_text}");
    (system, user)
}

/// map-reduce 的第一个阶段:分段摘要。
pub fn map_prompt(scene: Scene, chunk: &str, idx: usize, total: usize) -> (String, String) {
    let system = format!(
        "你是一个专业的录音纪要助手。现在给你的是完整录音的第 {idx}/{total} 段。\n\
         请只针对这一段提取要点,不要总结整场内容,也不要提及“本段”之外的任何信息。\n\n\
         输出要求:\n\
         - 列出这一段的主要要点(每条一行,可用子项)\n\
         - 标出其中的关键结论、数字、定义、作业/待办\n\
         - 保留时间戳(格式 [HH:MM:SS])\n\
         - 不要编造未提及的内容\n\
         场景背景:{}",
        scene.label()
    );
    let user = format!("第 {idx}/{total} 段转写:\n\n{chunk}");
    (system, user)
}

/// map-reduce 的第二个阶段:聚合。
///
/// **聚合 prompt 也必须按场景分支** —— 否则课堂会被聚合成干巴巴的要点堆砌,
/// 丢掉"这是给学生复习用的"这个本质。
pub fn reduce_prompt(scene: Scene, partials: &[String]) -> (String, String) {
    let system = format!(
        "你是一个专业的录音纪要助手。以下是同一场录音各分段的要点,请把它们合并成一份完整纪要。\n\n\
         合并要求:\n\
         - 去重:不同分段可能重复提到同一件事,只保留一次\n\
         - 重组:按主题归类,不要按分段顺序罗列\n\
         - 保留时间戳,便于回听原话\n\
         - 不要引入分段要点里没有的信息\n\n\
         {}\n\n{}",
        scene_structure(scene),
        COMMON_RULES
    );
    let mut user = String::from("各分段要点:\n\n");
    for (i, p) in partials.iter().enumerate() {
        user.push_str(&format!("=== 分段 {} ===\n{}\n\n", i + 1, p));
    }
    (system, user)
}

/// 术语校正 prompt。见技术方案 §10.8。
///
/// Whisper 在中文上有稳定的同音词错误(知觉/直觉、实效/时效),
/// 以及专业术语错认(注意力机制 → 注意立即制)。热词注入效果很弱,
/// 转写后校正才是有效手段,而且成本极低。
///
/// # 两条硬要求(都是实测踩出来的)
///
/// **① 必须明确要求输出简体。**
///
/// 转写那一步靠 `initial_prompt` 里的简体提示压住 Whisper 的繁体偏好,
/// 但纠错这一步**只送文字**,那个提示不在场。实测:一段繁体转写
/// (`堅歷視覺…`)纠错后仍然全是繁体。所以在 prompt 里再说一遍。
///
/// **② 术语表要说成"权威写法",不能只说"参考"。**
///
/// 原来的措辞是"参考术语表(可能出现的正确写法)"。实测 `坚歷視覺`
/// 本该改成 `计算机视觉`(术语表里有),LLM 却改成了 `建立视觉` ——
/// 一个读音相近但毫无意义的组合。它把术语表当成了"仅供参考"。
/// 改成"以此为准"之后才稳定生效。
pub fn term_correction_prompt(text: &str, terms: &[String]) -> (String, String) {
    let glossary = if terms.is_empty() {
        "(无)".to_string()
    } else {
        terms.join("、")
    };
    let system = format!(
        "你是一个中文语音转写校对助手。请修正转写文本中的错别字与术语错误。\n\n\
         严格约束:\n\
         1. **只做纠错,不改写句子** —— 不要润色、不要合并或拆分句子、不要调整语序。\n\
         2. **不要删除任何内容**,包括口语重复和语气词。\n\
         3. **不要添加任何新内容**,不要补充解释。\n\
         4. 拿不准的地方保持原样,宁可不改。\n\
         5. **必须输出简体中文。** 即使原文是繁体,也要转成简体。\n\
         6. 只输出修正后的文本,不要任何说明文字。\n\n\
         术语表(这些是**权威写法**。原文里凡出现读音相近、但写法不在表内的\n\
         词,一律改成表中的写法):{glossary}"
    );
    let user = format!("待校对文本:\n\n{text}");
    (system, user)
}

// ---------------------------------------------------------------------------
// 简略总结
// ---------------------------------------------------------------------------

/// 简略总结的结构。
///
/// 与详细总结的分工(见技术方案 §17.3):
/// - **详细**:完整结构,给"要复习 / 要执行"的人看
/// - **简略**:一页速览,给"只想回忆讲了什么"的人看
///
/// 所以简略版**不是详细版的压缩**,而是换了个目标:要点 + 结论 + 待办,一眼扫完。
/// 真正的层次结构交给思维导图。
pub fn brief_structure(scene: Scene) -> &'static str {
    match scene {
        Scene::Lecture => {
            "按以下结构输出 Markdown(全文 400 字以内):\n\
             ## 一句话\n(这门课讲了什么)\n\
             ## 核心要点\n(5~8 条,每条一行,只写结论不展开)\n\
             ## 需要记住\n(定义、公式、结论;没有就写“无”)\n\
             ## 作业与考试\n(没有就写“未提及”)"
        }
        Scene::Meeting => {
            "按以下结构输出 Markdown(全文 400 字以内):\n\
             ## 一句话\n(这个会议达成了什么)\n\
             ## 核心要点\n(5~8 条,每条一行)\n\
             ## 决议\n(没有就写“无”)\n\
             ## 待办\n(表格:任务 | 负责人 | 截止时间)"
        }
        Scene::Interview => {
            "按以下结构输出 Markdown(全文 400 字以内):\n\
             ## 一句话\n\
             ## 核心观点\n(5~8 条)\n\
             ## 结论"
        }
        Scene::Other => {
            "按以下结构输出 Markdown(全文 400 字以内):\n\
             ## 一句话\n\
             ## 核心要点\n(5~8 条)"
        }
    }
}

const BRIEF_RULES: &str = "写作要求:\n\
     1. **这是速览,不是详细纪要。** 每条只写结论,不要展开解释。\n\
     2. 只依据转写内容,不要编造。\n\
     3. 宁少勿滥:5 条准的胜于 10 条凑的。\n\
     4. 直接输出 Markdown,不要用代码块包裹。";

/// 简略总结(短音频,单次调用)。
pub fn brief_prompt(scene: Scene, transcript_text: &str) -> (String, String) {
    let system = format!(
        "你是一个录音速览助手。你要把课堂或会议压缩成一页能扫完的速览。\n\n{}\n\n{}",
        brief_structure(scene),
        BRIEF_RULES
    );
    let user = format!("以下是转写内容:\n\n{transcript_text}");
    (system, user)
}

/// 简略总结(map-reduce 的聚合阶段)。
pub fn brief_reduce_prompt(scene: Scene, partials: &[String]) -> (String, String) {
    let system = format!(
        "你是一个录音速览助手。以下是同一场录音各分段的要点,\
         请把它们压缩成一页能扫完的速览。\n\n\
         合并要求:\n\
         - 去重:同一件事只保留一次\n\
         - 只留最重要的 5~8 条\n\
         - 不要引入分段要点里没有的信息\n\n\
         {}\n\n{}",
        brief_structure(scene),
        BRIEF_RULES
    );
    let mut user = String::from("各分段要点:\n\n");
    for (i, p) in partials.iter().enumerate() {
        user.push_str(&format!("=== 分段 {} ===\n{}\n\n", i + 1, p));
    }
    (system, user)
}

// ---------------------------------------------------------------------------
// 思维导图(Mermaid)
// ---------------------------------------------------------------------------

/// 思维导图的 Mermaid 提示词。
///
/// **为什么用 Mermaid 而不是让模型画图:**
/// LLM 生成结构化文本比生成图片可靠得多,而且文本可版本控制、可手改。
///
/// **为什么把语法规则写死在提示里:**
/// 实测最常见的失败是节点名含特殊字符(括号、冒号)导致解析崩掉。
/// 与其事后修,不如事前约束。输出后还有 `check_mermaid` 兜底。
pub fn mindmap_prompt(scene: Scene, transcript_text: &str) -> (String, String) {
    let focus = match scene {
        Scene::Lecture => "按知识点层次组织:课程主题 → 各知识模块 → 关键概念 / 公式 / 例题",
        Scene::Meeting => "按议题层次组织:会议主题 → 各议题 → 结论 / 待办",
        Scene::Interview => "按主题层次组织:访谈主题 → 各话题 → 核心观点",
        Scene::Other => "按内容层次组织:主题 → 主要方面 → 关键细节",
    };

    let system = format!(
        "你是一个知识结构梳理助手。请把录音内容整理成一张**思维导图**,\
         用 Mermaid 的 mindmap 语法输出。\n\n\
         组织方式:{focus}\n\n\
         语法要求(**务必严格遵守,否则图会渲染失败**):\n\
         1. 第一行必须是 mindmap,不要有任何前置文字或代码块标记\n\
         2. 用**缩进**表示层级,每层缩进 2 个空格\n\
         3. 节点文本**不要包含**这些字符:圆括号 方括号 花括号 冒号 分号 引号 以及换行\n\
         4. 需要强调的根节点可以用双圆括号包住,但里面同样不能有上述字符\n\
         5. 层级控制在 3~4 层,同层节点不超过 7 个\n\
         6. 节点文字简短(不超过 15 字),细节留给文本大纲\n\n\
         只输出 Mermaid 源码本身,不要解释、不要代码块包裹。"
    );
    let user = format!("录音内容:\n\n{transcript_text}");
    (system, user)
}

/// 从模型输出里提取 Mermaid 源码。
///
/// 模型经常不听话地包上 ```mermaid 代码块,或在前后加解释文字。
pub fn extract_mermaid(raw: &str) -> String {
    let t = raw.trim();

    let mut s = t;
    for fence in ["```mermaid", "```Mermaid", "```mmd", "```"] {
        if let Some(rest) = s.strip_prefix(fence) {
            s = rest;
            break;
        }
    }
    if let Some(idx) = s.rfind("```") {
        s = &s[..idx];
    }
    let s = s.trim();

    // 从第一行合法的图类型开始截取(丢掉前面的解释文字)
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.iter().position(|l| {
        let l = l.trim().to_ascii_lowercase();
        l.starts_with("mindmap") || l.starts_with("graph") || l.starts_with("flowchart")
    });

    match start {
        Some(i) => lines[i..].join("\n").trim_end().to_string(),
        // 找不到合法开头就原样返回,让 check_mermaid 去报错
        None => s.to_string(),
    }
}

/// 把 Mermaid 源码转成缩进大纲(降级产物)。
///
/// 目的:即使图渲染不出来,用户仍然有一份可读的层次结构。
///
/// 缩进以**最少缩进的那一行**为基准 —— 这样 `root((x))` 在第 2 列缩进时,
/// 大纲不会整体多缩一级。
pub fn mermaid_to_outline(mermaid: &str) -> String {
    // 先收集(原始缩进, 文本),再统一归一化
    let mut items: Vec<(usize, String)> = Vec::new();
    let mut started = false;

    for line in mermaid.lines() {
        let trimmed = line.trim();
        if !started {
            let low = trimmed.to_ascii_lowercase();
            if low.starts_with("mindmap")
                || low.starts_with("graph")
                || low.starts_with("flowchart")
            {
                started = true;
            }
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with("%%") {
            continue;
        }
        // 只按空格计缩进(不用 byte 索引,避免中文字符干扰)
        let spaces = line.chars().take_while(|c| *c == ' ').count();
        let text = strip_mermaid_shape(trimmed);
        if text.is_empty() {
            continue;
        }
        items.push((spaces, text));
    }

    if items.is_empty() {
        return String::new();
    }

    let base = items.iter().map(|(i, _)| *i).min().unwrap_or(0);
    let mut out = String::new();
    for (spaces, text) in items {
        let level = (spaces.saturating_sub(base)) / 2;
        out.push_str(&"  ".repeat(level));
        out.push_str("- ");
        out.push_str(&text);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// 把 Mermaid 节点表达式剥成纯文本。
///
/// 处理三类:
/// - `((文本))` / `[文本]` / `{文本}` / `([文本])` → 文本
/// - `节点ID[文本]` → 文本(**丢掉节点 ID**,ID 只对 Mermaid 有意义)
/// - `A --> B` 这类连线 → 去掉箭头,保留两端
///
/// 公开是为了能被单独测试与复用。
pub fn strip_mermaid_shape(s: &str) -> String {
    let mut owned: String = s.trim().to_string();

    // 去掉行尾样式标记
    if let Some(i) = owned.find(":::") {
        owned.truncate(i);
    }

    // 先处理连线语法:graph 里 `A --> B` 表示关系,大纲里只要节点名
    if owned.contains("-->") || owned.contains("---") || owned.contains("->") {
        owned = owned
            .replace("-->", " ")
            .replace("---", " ")
            .replace("->", " ");
    }
    let mut t: &str = owned.trim();

    // 有方括号/花括号时,取出括号内的文本(丢掉前面的节点 ID)
    if let Some(text) = extract_bracket_text(t) {
        return text;
    }

    // 形如 `root((课程))` / `n1(课程)`:丢掉节点 ID,只留括号内容。
    //
    // 注意 `((` 是 Mermaid 的"圆形"形状标记,不是两个嵌套括号 ——
    // 所以这里先把连续的开括号一起吃掉。
    //
    // 这一步很重要:`repair_mermaid` 会给缺 ID 的形状节点补 `n1` 这样的占位 ID,
    // 而大纲是给人看的 —— `- n1(二叉树的遍历)` 显然不如 `- 二叉树的遍历`。
    if let Some(open) = t.find('(') {
        let id = t[..open].trim();
        let rest = &t[open..];
        let id_is_plain = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if id_is_plain && rest.ends_with(')') {
            t = rest.trim_start_matches('(').trim_end_matches(')').trim();
            while let Some(inner) = t
                .strip_prefix('(')
                .and_then(|x| x.strip_suffix(')'))
                .map(|x| x.trim())
            {
                if inner.is_empty() {
                    break;
                }
                t = inner;
            }
            return t.trim_start_matches(['-', '>', ' ']).trim().to_string();
        }
    }

    // 只有圆括号:逐层剥掉包裹的成对括号。
    //
    // ⚠️ 这里必须用 strip_prefix / strip_suffix —— 它们各只剥**一个**字符。
    //    用 trim_start_matches 会一次剥掉所有匹配字符:`((主题))` 会被剥成 `主题` 而不是
    //    `(主题)`,导致 `root((课程))` 这种嵌套标签剥不干净。
    loop {
        let stripped = t
            .strip_prefix('(')
            .and_then(|x| x.strip_suffix(')'))
            .map(|x| x.trim());
        match stripped {
            Some(inner) if !inner.is_empty() => t = inner,
            _ => break,
        }
    }

    // 去掉残留的箭头前缀
    t.trim_start_matches(['-', '>', ' ']).trim().to_string()
}

/// 取出 `id[文本]` / `id{文本}` 里的文本(丢掉前面的节点 ID)。
///
/// ⚠️ **不处理圆括号** —— 圆括号由调用方的剥括号循环处理。
///    早期版本把圆括号也纳进来,结果 `((主题))` 会被这里当成 `(主题)` 直接返回,
///    剥括号循环根本没机会执行。
///
/// 只取**最外层**的括号内容,并且要求括号配对:
/// `([文本])` → `[文本]`,再由下一轮调用剥成 `文本`。
fn extract_bracket_text(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return None;
    }

    // 从右往左找最外层的闭合括号
    let close = chars
        .iter()
        .rposition(|c| matches!(c, ']' | '}'))?;
    let open_char = match chars[close] {
        ']' => '[',
        '}' => '{',
        _ => return None,
    };

    // 找与之配对的起始括号
    let mut depth = 0i32;
    let mut open = None;
    for i in (0..=close).rev() {
        if chars[i] == chars[close] {
            depth += 1;
        } else if chars[i] == open_char {
            depth -= 1;
            if depth == 0 {
                open = Some(i);
                break;
            }
        }
    }
    let open = open?;

    let inner: String = chars[open + 1..close].iter().collect();
    let inner = inner.trim().to_string();
    if inner.is_empty() {
        None
    } else {
        Some(inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Segment;

    fn transcript(texts: &[&str]) -> Transcript {
        let segs: Vec<Segment> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| Segment::new(i as u64 * 1000, (i as u64 + 1) * 1000, *t))
            .collect();
        let raw = texts.join("");
        Transcript {
            engine: "mock".into(),
            model: "m".into(),
            backend: crate::types::Backend::Cpu,
            backend_diarize: None,
            language: Some("zh".into()),
            segments: segs,
            raw_text: raw,
            duration_ms: texts.len() as u64 * 1000,
            diarize: None,
        }
    }

    #[test]
    fn token_estimate_counts_cjk_per_char() {
        assert_eq!(estimate_tokens("你好世界"), 4);
        // ASCII 大约 4 字符 1 token
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
    }

    #[test]
    fn token_estimate_zero_for_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn scene_samples_include_all_three_regions_when_long() {
        // 造一段很长的转写,三段的文本各自不同,便于断言都出现了
        let mut texts = vec!["开头的独特内容ABCDEFG"];
        for _ in 0..400 {
            texts.push("中间填充内容甲乙丙丁戊己庚辛壬癸");
        }
        texts.push("末尾的独特内容ZYXWVUT");
        let t = transcript(&texts);

        let s = scene_samples(&t);
        assert!(s.contains("【开头】"));
        assert!(s.contains("【中段】"));
        assert!(s.contains("【末尾】"));
        assert!(s.contains("开头的独特内容"), "必须采样开头");
        assert!(s.contains("末尾的独特内容"), "★ 必须采样末尾 —— 只看开头会被闲聊/点名误导");
    }

    #[test]
    fn scene_samples_returns_all_when_short() {
        let t = transcript(&["短文本"]);
        let s = scene_samples(&t);
        assert!(s.contains("短文本"));
        assert!(!s.contains("【中段】"), "短音频不必分段采样");
    }

    #[test]
    fn parse_scene_verdict_plain_json() {
        let v = parse_scene_verdict(
            r#"{"scene":"lecture","confidence":0.9,"evidence":"有作业","suggested_focus":["知识点"]}"#,
        )
        .unwrap();
        assert_eq!(v.scene, Scene::Lecture);
        assert!((v.confidence - 0.9).abs() < 1e-6);
        assert_eq!(v.suggested_focus, vec!["知识点"]);
    }

    #[test]
    fn parse_scene_verdict_tolerates_markdown_fence() {
        let v = parse_scene_verdict(
            "```json\n{\"scene\":\"meeting\",\"confidence\":0.8}\n```",
        )
        .unwrap();
        assert_eq!(v.scene, Scene::Meeting);
        assert_eq!(v.evidence, "");
    }

    #[test]
    fn parse_scene_verdict_tolerates_surrounding_prose() {
        let v = parse_scene_verdict(
            "好的,我的判断如下:\n{\"scene\":\"interview\",\"confidence\":0.7}\n希望有帮助。",
        )
        .unwrap();
        assert_eq!(v.scene, Scene::Interview);
    }

    #[test]
    fn parse_scene_verdict_unknown_scene_falls_back_to_other() {
        let v = parse_scene_verdict(r#"{"scene":"podcast","confidence":0.9}"#).unwrap();
        assert_eq!(v.scene, Scene::Other);
    }

    #[test]
    fn parse_scene_verdict_clamps_confidence() {
        let v = parse_scene_verdict(r#"{"scene":"lecture","confidence":5.0}"#).unwrap();
        assert_eq!(v.confidence, 1.0);
        let v = parse_scene_verdict(r#"{"scene":"lecture","confidence":-1.0}"#).unwrap();
        assert_eq!(v.confidence, 0.0);
    }

    #[test]
    fn parse_scene_verdict_defaults_confidence_when_missing() {
        let v = parse_scene_verdict(r#"{"scene":"lecture"}"#).unwrap();
        assert!((v.confidence - 0.5).abs() < 1e-6);
    }

    #[test]
    fn parse_scene_verdict_errors_without_json() {
        assert!(parse_scene_verdict("我觉得这是一节课").is_err());
    }

    #[test]
    fn extract_json_handles_nested_objects_and_strings_with_braces() {
        let s = r#"{"a":{"b":1},"c":"contains } brace"}"#;
        let got = extract_json_object(s).unwrap();
        assert_eq!(got, s);
    }

    #[test]
    fn extract_json_handles_escaped_quotes() {
        let s = r#"{"a":"he said \"hi\" }"}"#;
        let got = extract_json_object(s).unwrap();
        assert_eq!(got, s);
    }

    #[test]
    fn scene_structures_differ_by_scene() {
        // ★ 课堂和会议的纪要结构必须不同,否则场景判断没有意义
        let lec = scene_structure(Scene::Lecture);
        let meet = scene_structure(Scene::Meeting);
        assert_ne!(lec, meet);
        assert!(lec.contains("复习提纲"));
        assert!(lec.contains("知识点"));
        assert!(meet.contains("待办"));
        assert!(meet.contains("决议"));
        assert!(!meet.contains("复习提纲"));
    }

    #[test]
    fn reduce_prompt_is_scene_specific() {
        // ★ 聚合 prompt 也必须按场景分支,否则课堂会被聚合成干巴巴的要点堆砌
        let (s_lec, _) = reduce_prompt(Scene::Lecture, &["a".into()]);
        let (s_meet, _) = reduce_prompt(Scene::Meeting, &["a".into()]);
        assert!(s_lec.contains("复习提纲"));
        assert!(s_meet.contains("待办"));
        assert_ne!(s_lec, s_meet);
    }

    #[test]
    fn single_prompt_puts_stable_content_in_system() {
        // ★ 稳定内容(模板+规则)必须在 system 里、正文在 user 里,
        //   这样 DeepSeek 的前缀缓存才能命中
        let (system, user) = single_prompt(Scene::Lecture, "这里是正文");
        assert!(system.contains("复习提纲"), "模板应在 system 中");
        assert!(!system.contains("这里是正文"), "正文不应出现在 system 中");
        assert!(user.contains("这里是正文"));
    }

    #[test]
    fn map_prompt_keeps_chunk_index() {
        let (system, user) = map_prompt(Scene::Lecture, "段落内容", 2, 5);
        assert!(system.contains("2/5"));
        assert!(user.contains("段落内容"));
        assert!(system.contains("不要总结整场"), "分段摘要不应越界总结");
    }

    #[test]
    fn term_correction_prompt_forbids_rewriting() {
        let (system, _) = term_correction_prompt("文本", &["反向传播".into()]);
        assert!(system.contains("只做纠错"));
        assert!(system.contains("不要删除任何内容"));
        assert!(system.contains("反向传播"));
    }

    #[test]
    fn term_correction_handles_empty_glossary() {
        let (system, _) = term_correction_prompt("文本", &[]);
        assert!(system.contains("(无)"), "{system}");
    }

    /// ★ 回归测试:纠错必须**明确要求输出简体**。
    ///
    /// 转写那步靠 `initial_prompt` 里的简体提示压住 Whisper 的繁体偏好,
    /// 但纠错只送文字,那个提示不在场。实测一段繁体转写纠错后仍然是繁体 ——
    /// 所以这条约束必须写在纠错 prompt 自己里。
    #[test]
    fn term_correction_demands_simplified_chinese() {
        let (system, _) = term_correction_prompt("文本", &[]);
        assert!(system.contains("简体"), "必须要求简体:{system}");
    }

    /// ★ 回归测试:术语表要说成**权威写法**。
    ///
    /// 原来写的是"参考术语表(可能出现的正确写法)"。实测 `坚歷視覺`
    /// 本该改成 `计算机视觉`(表里有),LLM 却改成了 `建立视觉` ——
    /// 一个读音相近但无意义的组合。它把术语表当成了"仅供参考"。
    #[test]
    fn term_correction_makes_glossary_authoritative() {
        let (system, _) = term_correction_prompt("文本", &["计算机视觉".into()]);
        assert!(system.contains("计算机视觉"), "术语应出现在 prompt 里");
        assert!(
            system.contains("权威") || system.contains("以此为准") || system.contains("一律改"),
            "术语表必须被说成权威写法,而不是仅供参考:{system}"
        );
    }

    // --- 简略总结 ----------------------------------------------------------

    #[test]
    fn brief_structure_differs_from_detailed() {
        // ★ 简略版不是详细版的压缩 —— 它换了目标(一页速览)
        for scene in [Scene::Lecture, Scene::Meeting, Scene::Interview, Scene::Other] {
            let brief = brief_structure(scene);
            let detailed = scene_structure(scene);
            assert_ne!(brief, detailed, "{scene:?} 的简略与详细结构不应相同");
        }
    }

    #[test]
    fn brief_structure_limits_length() {
        // 简略版必须有字数约束,否则模型会写成长篇
        for scene in [Scene::Lecture, Scene::Meeting, Scene::Interview, Scene::Other] {
            let s = brief_structure(scene);
            assert!(s.contains("400 字以内"), "{scene:?}: {s}");
        }
    }

    #[test]
    fn brief_prompt_forbids_expansion() {
        let (system, user) = brief_prompt(Scene::Lecture, "正文");
        assert!(system.contains("速览"), "{system}");
        assert!(system.contains("不要展开解释"), "{system}");
        assert!(system.contains("宁少勿滥"), "{system}");
        assert!(user.contains("正文"));
    }

    #[test]
    fn brief_reduce_prompt_is_scene_specific() {
        let (lec, _) = brief_reduce_prompt(Scene::Lecture, &["a".into()]);
        let (meet, _) = brief_reduce_prompt(Scene::Meeting, &["a".into()]);
        assert!(lec.contains("作业与考试"), "{lec}");
        assert!(meet.contains("待办"), "{meet}");
    }

    // --- 思维导图 ----------------------------------------------------------

    #[test]
    fn mindmap_prompt_constrains_syntax() {
        let (system, user) = mindmap_prompt(Scene::Lecture, "内容");
        // 必须把语法约束写死,否则模型会生成渲染不出来的图
        assert!(system.contains("第一行必须是 mindmap"), "{system}");
        assert!(system.contains("缩进"), "{system}");
        assert!(system.contains("不要包含"), "{system}");
        assert!(user.contains("内容"));
    }

    #[test]
    fn mindmap_prompt_differs_by_scene() {
        let (lec, _) = mindmap_prompt(Scene::Lecture, "x");
        let (meet, _) = mindmap_prompt(Scene::Meeting, "x");
        assert!(lec.contains("知识点"), "{lec}");
        assert!(meet.contains("议题"), "{meet}");
    }

    #[test]
    fn extract_mermaid_strips_code_fence() {
        let raw = "```mermaid\nmindmap\n  root((主题))\n    要点\n```";
        let m = extract_mermaid(raw);
        assert!(m.starts_with("mindmap"), "{m}");
        assert!(!m.contains("```"), "{m}");
    }

    #[test]
    fn extract_mermaid_strips_surrounding_prose() {
        let raw = "好的,这是思维导图:\n\nmindmap\n  root((主题))\n    要点\n\n希望有帮助!";
        let m = extract_mermaid(raw);
        assert!(m.starts_with("mindmap"), "{m}");
        assert!(!m.contains("好的"), "{m}");
    }

    #[test]
    fn extract_mermaid_preserves_content_when_no_header() {
        // 没有合法开头时原样返回,让 check_mermaid 去报错
        let raw = "这是一段说明文字";
        assert_eq!(extract_mermaid(raw), "这是一段说明文字");
    }

    #[test]
    fn mermaid_to_outline_builds_hierarchy() {
        let mm = "mindmap\n  root((课程))\n    模块一\n      概念A\n      概念B\n    模块二";
        let out = mermaid_to_outline(mm);
        // 节点 ID 与形状标记都被剥掉,只留文本
        assert!(out.contains("- 课程"), "{out}");
        assert!(out.contains("  - 模块一"), "一级应有缩进: {out}");
        assert!(out.contains("    - 概念A"), "二级应有缩进: {out}");
        assert!(!out.contains("root"), "节点 ID 应被剥掉: {out}");
        assert!(!out.contains("(("), "形状标记应被剥掉: {out}");
    }

    #[test]
    fn strip_shape_drops_node_id_for_round_shape() {
        // 回归测试:`root((课程))` 曾因为循环遇到首字符是 `r` 而完全不执行,
        // 导致括号原样留在输出里。
        assert_eq!(strip_mermaid_shape("root((课程))"), "课程");
        assert_eq!(strip_mermaid_shape("root(课程)"), "课程");
        assert_eq!(strip_mermaid_shape("a1((节点))"), "节点");
        // repair_mermaid 补的占位 ID 也不该出现在大纲里
        assert_eq!(strip_mermaid_shape("n1((二叉树的遍历))"), "二叉树的遍历");
        assert_eq!(strip_mermaid_shape("n12(中序遍历)"), "中序遍历");
    }

    #[test]
    fn strip_shape_keeps_text_that_only_looks_like_an_id() {
        // 中文短语不该被当成节点 ID 丢掉
        assert_eq!(strip_mermaid_shape("这是一个节点"), "这是一个节点");
        // 带空格的也不是 ID
        assert_eq!(strip_mermaid_shape("hello world"), "hello world");
    }

    #[test]
    fn mermaid_to_outline_normalizes_base_indent() {
        // 根节点缩进多少不影响结果 —— 以最少缩进的那行为基准
        let a = mermaid_to_outline("mindmap\n  root(x)\n    子节点");
        let b = mermaid_to_outline("mindmap\n        root(x)\n          子节点");
        assert_eq!(a, b, "缩进基准应归一化");
        assert!(a.starts_with("- "), "最外层不应有多余缩进: {a}");
    }

    #[test]
    fn mermaid_to_outline_handles_graph_syntax() {
        let mm = "graph TD\n  A[开始]\n  B{判断}\n  A --> B";
        let out = mermaid_to_outline(mm);
        assert!(out.contains("开始"), "{out}");
        assert!(out.contains("判断"), "{out}");
        assert!(!out.contains('['), "{out}");
        assert!(!out.contains('{'), "{out}");
    }

    #[test]
    fn mermaid_to_outline_empty_input() {
        assert_eq!(mermaid_to_outline(""), "");
        assert_eq!(mermaid_to_outline("mindmap"), "");
    }

    #[test]
    fn mermaid_to_outline_strips_style_suffix() {
        let mm = "mindmap\n  root((主题))\n    节点:::highlight";
        let out = mermaid_to_outline(mm);
        assert!(out.contains("节点"), "{out}");
        assert!(!out.contains(":::"), "{out}");
    }

    #[test]
    fn strip_mermaid_shape_nested_wrappers() {
        assert_eq!(strip_mermaid_shape("((主题))"), "主题");
        assert_eq!(strip_mermaid_shape("[节点]"), "节点");
        assert_eq!(strip_mermaid_shape("{判断}"), "判断");
        assert_eq!(strip_mermaid_shape("(([混合]))"), "混合");
        assert_eq!(strip_mermaid_shape("裸文本"), "裸文本");
        assert_eq!(strip_mermaid_shape(""), "");
    }

    #[test]
    fn strip_mermaid_shape_handles_arrows() {
        assert_eq!(strip_mermaid_shape("--> 目标"), "目标");
        assert_eq!(strip_mermaid_shape("-> 目标"), "目标");
    }

    #[test]
    fn map_reduce_split_keeps_all_content() {
        let text = (0..2000)
            .map(|i| format!("这是第{i}句话。"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = split_for_map_reduce(&text);
        assert!(chunks.len() > 1, "长文本应被切分");
        // 拼接后应覆盖全部内容(不丢句)
        let rejoined: String = chunks.join("\n");
        for i in [0, 500, 1999] {
            assert!(
                rejoined.contains(&format!("这是第{i}句话。")),
                "切分不能丢内容:第 {i} 句"
            );
        }
    }

    #[test]
    fn map_reduce_split_prefers_sentence_boundary() {
        let text = (0..1200)
            .map(|i| format!("句子{i}。"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = split_for_map_reduce(&text);
        // 除最后一段外,每段都应以句末标点收尾
        for c in &chunks[..chunks.len().saturating_sub(1)] {
            let last = c.trim_end().chars().last().unwrap();
            assert!(
                matches!(last, '。' | '!' | '?' | '…' | '.' | ';'),
                "分段应在句子边界结束,实际以 {last:?} 结尾"
            );
        }
    }

    #[test]
    fn map_reduce_split_short_text_is_single_chunk() {
        let chunks = split_for_map_reduce("很短的一段内容。");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "很短的一段内容。");
    }

    #[test]
    fn map_reduce_split_empty_input() {
        assert!(split_for_map_reduce("").is_empty());
    }

    #[test]
    fn map_reduce_split_caps_chunk_count() {
        // 造一个巨大的文本,确保段数不会无限增长
        let text = (0..200_000)
            .map(|i| format!("第{i}句。"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = split_for_map_reduce(&text);
        assert!(
            chunks.len() <= 40,
            "分段数应有上限(避免请求爆炸),实际 {}",
            chunks.len()
        );
    }

    #[test]
    fn map_reduce_split_handles_single_huge_line() {
        let huge = "字".repeat(20_000);
        let chunks = split_for_map_reduce(&huge);
        assert!(!chunks.is_empty());
        let total: usize = chunks.iter().map(|c| c.chars().count()).sum();
        assert_eq!(total, 20_000, "超长单行也不能丢字");
    }
}
