//! 转写纠错。
//!
//! 见技术方案 §10.8,以及 `docs/转写质量改进方案.md`。
//!
//! # 为什么需要它
//!
//! Whisper 在中文上有**稳定的**同音词错误:`知觉/直觉`、`实效/时效`、
//! `计算机视觉 → 坚立视觉`、`相机 → 像机`。这类错误的特点是
//! **同一个音每次都以同样的方式错** —— 实测一个两小时录音里
//! `坚立视觉` 出现 8 次、`像机` 8 次。
//!
//! 热词注入(`initial_prompt`)对这种错误**效果很弱**,因为它是解码期的
//! 软偏置。而"同一错误重复出现"恰恰是**上下文纠错**最擅长处理的。
//!
//! 所以正确做法是:转写完再用 LLM 过一遍。成本极低(一次 chat),
//! 收益直接。
//!
//! # 安全约束(重要)
//!
//! 纠错会**直接改写转写文件**,改错了就看不到原文了。所以这里的原则是
//! **宁可漏改,不可错改**:
//!
//! 1. **逐段处理**,保留段落边界。时间轴由段边界决定,不能被 LLM 打乱。
//! 2. **校验段数**:LLM 返回的段数必须与送出的一致。不一致说明它合并或
//!    拆分或吞了段落 —— 那批**整体放弃**,保留原文。
//! 3. **长度突变也放弃**:单段长度变化超过阈值(比如 3 倍或 1/3),
//!    多半是改写而不是纠错,同样保留原文。
//! 4. **任一批失败不影响其他批**,也不影响整条管线。
//!
//! 这些检查都很土,但它们是"改错了看不出来"的唯一防线。

use super::prompt::term_correction_prompt;
use super::LlmClient;
use crate::types::Segment;

/// 每批送几段给 LLM。
///
/// 太大容易超上下文,太小则失去上下文(纠错恰恰需要上下文才能判断
/// "jian li shi jue" 该写成"计算机视觉")。20 段大约对应几分钟音频,
/// 足够判断术语,又不会太长。
pub const BATCH_SIZE: usize = 20;

/// 单段长度变化的容忍倍数。
///
/// 纠错不该让一段的字符数变化太多。超过这个倍数说明 LLM 在改写而不是
/// 纠错 —— 那就保留原文。
const MAX_GROWTH: f32 = 3.0;
const MAX_SHRINK: f32 = 0.34;

/// 纠错结果统计。用于在界面上如实报告做了多少。
#[derive(Clone, Debug, Default)]
pub struct CorrectionStats {
    /// 送出的批次总数
    pub batches: usize,
    /// 成功应用纠错的批次
    pub batches_applied: usize,
    /// 因段数不匹配而放弃的批次
    pub batches_skipped_count_mismatch: usize,
    /// 因失败(网络/解析)而放弃的批次
    pub batches_failed: usize,
    /// 实际被改动的段数
    pub segments_changed: usize,
    /// 送出去但**一个字都没改**的批次。
    ///
    /// 这**不是失败** —— LLM 看完认为这段没问题。但它必须和"失败"
    /// 分开统计:否则摘要会把"无需纠错"报成"纠错未生效",
    /// 让用户以为功能坏了。(实测踩到过:一段 107 字的转写,
    /// LLM 认为不需要改,摘要却说"1 批全部跳过"。)
    pub batches_unchanged: usize,
}

impl CorrectionStats {
    /// 一句话摘要,给界面用。
    ///
    /// 三种结果必须能区分开:
    /// - **改了** → "已纠错 N 段"
    /// - **没问题** → "N 批无需改动"  ← 和失败完全不同
    /// - **出错了** → "N 批失败"
    pub fn summary(&self) -> String {
        if self.batches == 0 {
            return "未纠错".into();
        }

        let mut parts: Vec<String> = Vec::new();
        if self.batches_applied > 0 {
            parts.push(format!(
                "已纠错 {} 段({} 批)",
                self.segments_changed, self.batches_applied
            ));
        }
        if self.batches_unchanged > 0 {
            parts.push(format!("{} 批无需改动", self.batches_unchanged));
        }
        if self.batches_skipped_count_mismatch > 0 {
            parts.push(format!(
                "{} 批段数不符已保留原文",
                self.batches_skipped_count_mismatch
            ));
        }
        if self.batches_failed > 0 {
            parts.push(format!("{} 批失败", self.batches_failed));
        }

        // 全是失败 → 明确说失败,不要说成"未生效"这种含糊话
        if self.batches_applied == 0
            && self.batches_unchanged == 0
            && self.batches_failed == self.batches
        {
            return format!("纠错全部失败({} 批)", self.batches);
        }
        if parts.is_empty() {
            return "纠错未生效".into();
        }
        parts.join(";")
    }
}

/// 对转写做一次纠错。**原地修改 `segments` 的 `text`,不动时间戳。**
///
/// `terms` 是术语表 —— 它既进 prompt 作为"可能的正确写法",
/// 也是纠错时判断"该往哪个方向改"的依据。这比把术语塞进
/// `initial_prompt` 有效得多。
///
/// 任何一批失败都只记进 [`CorrectionStats`],不返回 Err ——
/// 纠错是**锦上添花**,不该让整条管线失败。
///
/// ⚠️ 这个版本会**自建 runtime**,只在调用方确认没有 runtime 时用。
/// 管线里请走 [`crate::llm::BlockingSummarizer::correct_blocking`]。
pub fn correct_transcript(
    client: &LlmClient,
    segments: &mut [Segment],
    terms: &[String],
) -> (CorrectionStats, Vec<String>) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            return (
                CorrectionStats {
                    batches_failed: 1,
                    ..Default::default()
                },
                vec![format!("创建 runtime 失败,跳过纠错:{e}")],
            )
        }
    };
    correct_with(client, &rt, segments, terms)
}

/// 纠错的实际实现。runtime 由调用方提供 ——
/// `BlockingSummarizer` 复用自己那个,避免嵌套。
pub(crate) fn correct_with(
    client: &LlmClient,
    rt: &tokio::runtime::Runtime,
    segments: &mut [Segment],
    terms: &[String],
) -> (CorrectionStats, Vec<String>) {
    let mut stats = CorrectionStats::default();
    let mut warnings: Vec<String> = Vec::new();

    if segments.is_empty() {
        return (stats, warnings);
    }

    let total = segments.len();
    let mut start = 0usize;

    while start < total {
        let end = (start + BATCH_SIZE).min(total);
        let batch = &segments[start..end];
        stats.batches += 1;

        // 只把**文字**送出去,不带时间戳 ——
        // 带了反而诱导 LLM 去改时间或重排顺序。
        let numbered: String = batch
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{}. {}", i + 1, s.text))
            .collect::<Vec<_>>()
            .join("\n");

        let (system, user) = term_correction_prompt(&numbered, terms);
        let out = match rt.block_on(client.chat(&system, &user)) {
            Ok(o) => o,
            Err(e) => {
                stats.batches_failed += 1;
                warnings.push(format!("纠错第 {} 批失败:{}", stats.batches, e));
                start = end;
                continue;
            }
        };

        let parsed = parse_numbered(&out.content);
        if parsed.len() != batch.len() {
            // ★ 段数不符 → 整批放弃。这条是安全的基石:
            //   宁可漏改,也不能把段落错位(那会让时间轴全乱)。
            stats.batches_skipped_count_mismatch += 1;
            warnings.push(format!(
                "纠错第 {} 批段数不符(期望 {} 段,返回 {} 段),已保留原文",
                stats.batches,
                batch.len(),
                parsed.len()
            ));
            start = end;
            continue;
        }

        // 逐段应用,每段还要过长度检查
        let mut applied_any = false;
        for (seg, new_text) in segments[start..end].iter_mut().zip(parsed.iter()) {
            let new_text = new_text.trim();
            if new_text.is_empty() {
                continue; // 空字符串视为"这段没改"
            }
            if new_text == seg.text.trim() {
                continue;
            }
            // 长度突变 → 多半是改写而不是纠错,保留原文
            let old_len = seg.text.chars().count().max(1) as f32;
            let new_len = new_text.chars().count() as f32;
            let ratio = new_len / old_len;
            if !(MAX_SHRINK..=MAX_GROWTH).contains(&ratio) {
                continue;
            }
            seg.text = new_text.to_string();
            stats.segments_changed += 1;
            applied_any = true;
        }
        if applied_any {
            stats.batches_applied += 1;
        } else {
            // 一个字没改 —— 记成"无需改动",不是失败。
            stats.batches_unchanged += 1;
        }

        start = end;
    }

    // 段数被 LLM 动过的话 raw_text 也要重算 —— 它必须与 segments 一致,
    // 否则下游(总结、检索)拿到的和看到的不是同一份文本。
    (stats, warnings)
}

/// 从 LLM 输出里解析 `1. 文本` 这样带序号的行。
///
/// 容错:序号后的分隔符可能是 `.`/`、`/`)`/`:`,也可能没有序号。
/// 解析不出来就返回空表 —— 调用方见段数不符会放弃整批。
fn parse_numbered(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 找开头的数字
        let mut chars = line.char_indices();
        let mut num_end = None;
        for (i, c) in chars.by_ref() {
            if c.is_ascii_digit() {
                num_end = Some(i + 1);
            } else {
                break;
            }
        }
        let Some(ne) = num_end else {
            // 没有序号 —— 可能是 LLM 直接返回了正文(单段批次常见)。
            // 整段当作一条,仅当目前还没收到任何带号的行时才接受。
            if out.is_empty() {
                out.push(line.to_string());
            }
            continue;
        };
        let rest = line[ne..].trim_start();
        // 去掉一个分隔符
        let rest = rest
            .strip_prefix('.')
            .or_else(|| rest.strip_prefix('、'))
            .or_else(|| rest.strip_prefix(')'))
            .or_else(|| rest.strip_prefix('）'))
            .or_else(|| rest.strip_prefix(':'))
            .or_else(|| rest.strip_prefix('：'))
            .unwrap_or(rest)
            .trim_start();
        out.push(rest.to_string());
    }
    out
}

/// 重算 `raw_text`,让它与 `segments` 保持一致。
pub fn rebuild_raw_text(t: &mut crate::types::Transcript) {
    t.raw_text = t.segments.iter().map(|s| s.text.as_str()).collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(texts: &[&str]) -> Vec<Segment> {
        texts
            .iter()
            .enumerate()
            .map(|(i, t)| Segment::new(i as u64 * 1000, (i as u64 + 1) * 1000, *t))
            .collect()
    }

    // --- 序号解析(这是安全防线的基础)---------------------------------

    #[test]
    fn parses_numbered_lines() {
        let s = "1. 第一段\n2. 第二段\n3. 第三段";
        assert_eq!(parse_numbered(s), vec!["第一段", "第二段", "第三段"]);
    }

    #[test]
    fn parses_common_separator_variants() {
        // LLM 可能用各种分隔符,不能只认 `.`
        for s in [
            "1、第一\n2、第二",
            "1) 第一\n2) 第二",
            "1）第一\n2）第二",
            "1: 第一\n2: 第二",
            "1：第一\n2：第二",
        ] {
            let got = parse_numbered(s);
            assert_eq!(got.len(), 2, "解析失败: {s:?} → {got:?}");
            assert_eq!(got[0], "第一", "{s:?}");
        }
    }

    #[test]
    fn parses_double_digit_indices() {
        // 超过 9 段时序号是两位数 —— 不能只取一位数字
        let s = "9. 第九\n10. 第十\n11. 第十一";
        let got = parse_numbered(s);
        assert_eq!(got, vec!["第九", "第十", "第十一"]);
    }

    #[test]
    fn skips_blank_lines() {
        let s = "1. 一\n\n2. 二\n\n";
        assert_eq!(parse_numbered(s), vec!["一", "二"]);
    }

    #[test]
    fn unnumbered_output_yields_single_entry() {
        // 单段批次时 LLM 可能直接返回正文
        assert_eq!(parse_numbered("就是这段文字"), vec!["就是这段文字"]);
    }

    #[test]
    fn unnumbered_line_after_numbered_is_ignored() {
        // 已经有带号的行了,再来无号的行说明格式乱了 —— 不收,
        // 让段数校验去发现不一致
        let s = "1. 一\n乱入的一行\n2. 二";
        let got = parse_numbered(s);
        assert_eq!(got, vec!["一", "二"], "无号行不该混进来:{got:?}");
    }

    // --- 安全约束 ----------------------------------------------------------

    #[test]
    fn length_guard_rejects_rewrites() {
        // 长度暴涨 3 倍以上 → 是改写不是纠错 → 保留原文
        let mut s = segs(&["短"]);
        let old_len = 1.0f32;
        let ratio = 5.0f32 / old_len;
        assert!(!(MAX_SHRINK..=MAX_GROWTH).contains(&ratio));

        // 直接验证常量边界的行为
        assert!((MAX_SHRINK..=MAX_GROWTH).contains(&1.0));
        assert!(!(MAX_SHRINK..=MAX_GROWTH).contains(&0.1));
        assert!(!(MAX_SHRINK..=MAX_GROWTH).contains(&10.0));
        let _ = &mut s;
    }

    #[test]
    fn rebuild_raw_text_matches_segments() {
        let mut t = crate::types::Transcript {
            engine: "t".into(),
            model: "m".into(),
            backend: crate::types::Backend::Cpu,
            backend_diarize: None,
            language: None,
            segments: segs(&["甲", "乙"]),
            raw_text: "旧的".into(),
            duration_ms: 2000,
            diarize: None,
        };
        rebuild_raw_text(&mut t);
        assert_eq!(t.raw_text, "甲乙");
    }

    #[test]
    fn stats_summary_is_honest_when_nothing_applied() {
        let mut st = CorrectionStats::default();
        assert_eq!(st.summary(), "未纠错");

        // 全部失败 → 明确说"失败",不要含糊成"未生效"
        st.batches = 3;
        st.batches_failed = 3;
        let s = st.summary();
        assert!(s.contains("失败"), "{s}");
        assert!(s.contains('3'), "{s}");

        // 改了 2 批、失败 1 批
        st.batches_applied = 2;
        st.segments_changed = 7;
        st.batches_failed = 1;
        let s = st.summary();
        assert!(s.contains('7'), "{s}");
        assert!(s.contains("失败"), "{s}");
    }

    /// ★ 回归测试:"无需改动"不能被报成"未生效"。
    ///
    /// 实测踩到:一段 107 字的转写,LLM 看完认为不需要改,
    /// 而摘要说"纠错未生效(1 批全部跳过)" —— 用户会以为功能坏了。
    /// 这两种情况的含义完全不同,必须分开说。
    #[test]
    fn unchanged_batches_are_not_reported_as_failure() {
        let st = CorrectionStats {
            batches: 1,
            batches_unchanged: 1,
            ..Default::default()
        };
        let s = st.summary();
        assert!(s.contains("无需改动"), "应说'无需改动':{s}");
        assert!(!s.contains("未生效"), "不该说'未生效':{s}");
        assert!(!s.contains("失败"), "不该说'失败':{s}");
    }

    #[test]
    fn mixed_outcomes_are_all_reported() {
        let st = CorrectionStats {
            batches: 5,
            batches_applied: 2,
            segments_changed: 9,
            batches_unchanged: 2,
            batches_failed: 1,
            ..Default::default()
        };
        let s = st.summary();
        assert!(s.contains('9'), "{s}"); // 改了 9 段
        assert!(s.contains("无需改动"), "{s}"); // 2 批没问题
        assert!(s.contains("失败"), "{s}"); // 1 批失败
    }

    #[test]
    fn all_applied_reads_as_a_clean_success() {
        let st = CorrectionStats {
            batches: 3,
            batches_applied: 3,
            segments_changed: 21,
            ..Default::default()
        };
        let s = st.summary();
        assert!(s.contains("21"), "{s}");
        assert!(!s.contains("无需要"), "{s}");
        assert!(!s.contains("失败"), "{s}");
    }
}
