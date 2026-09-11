//! 转写的三种视图与段落合并。见技术方案 §6。
//!
//! 核心原则:**存储结构化、显示靠派生**。
//! 底层 `Vec<Segment>` 是唯一真相源;对话体/时间轴/纯文本都是它的视图,
//! 合并只影响展示,原始段落永远不动。

use crate::types::{Segment, SpeakerLabels};

/// 一段合并后的发言。
#[derive(Clone, Debug)]
pub struct Utterance {
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker_id: Option<u32>,
    pub text: String,
    /// 是否含重叠语音(抢话)。UI 可以用样式区分。
    pub overlapped: bool,
}

/// 段落合并策略。
///
/// Whisper 会把一句话切成好几段,直接显示会碎成一行一句。
/// 合并是纯展示层逻辑,底层 Segment 永远不动。
#[derive(Clone, Copy, Debug)]
pub struct MergePolicy {
    /// 相邻段最大间隔,超过就不合并(说话人停顿了)
    pub max_gap_ms: u64,
    /// 合并后单条最长时长
    pub max_duration_ms: u64,
    /// 合并后单条最长字数
    pub max_chars: usize,
}

impl Default for MergePolicy {
    fn default() -> Self {
        Self {
            max_gap_ms: 2500,
            max_duration_ms: 60_000,
            max_chars: 200,
        }
    }
}

/// 判断某个字符是否是句末标点。
fn ends_sentence(s: &str) -> bool {
    matches!(
        s.chars().last(),
        Some('。') | Some('!') | Some('?') | Some('…') | Some('.')
    )
}

/// 把段落合并成对话体。
pub fn merge_for_dialogue(segments: &[Segment], policy: &MergePolicy) -> Vec<Utterance> {
    let mut out: Vec<Utterance> = Vec::new();

    for seg in segments {
        if seg.text.trim().is_empty() {
            continue;
        }
        let can_merge = match out.last() {
            None => false,
            Some(last) => {
                last.speaker_id == seg.speaker_id
                    && seg.start_ms.saturating_sub(last.end_ms) < policy.max_gap_ms
                    && seg.end_ms.saturating_sub(last.start_ms) <= policy.max_duration_ms
                    && last.text.chars().count() + seg.text.chars().count() <= policy.max_chars
                    // 上一段已经以句末标点结束 → 不合并,保持句子边界
                    && !ends_sentence(&last.text)
            }
        };

        if can_merge {
            let last = out.last_mut().unwrap();
            last.text.push_str(&seg.text);
            last.end_ms = seg.end_ms;
            last.overlapped |= seg.overlapped;
        } else {
            out.push(Utterance {
                start_ms: seg.start_ms,
                end_ms: seg.end_ms,
                speaker_id: seg.speaker_id,
                text: seg.text.clone(),
                overlapped: seg.overlapped,
            });
        }
    }

    out
}

/// 用默认策略合并。多数调用方用这个。
pub fn merge_for_dialogue_default(segments: &[Segment]) -> Vec<Utterance> {
    merge_for_dialogue(segments, &MergePolicy::default())
}

// ---------------------------------------------------------------------------
// 时间格式化
// ---------------------------------------------------------------------------

/// 毫秒 → `HH:MM:SS`。
pub fn format_timestamp(ms: u64) -> String {
    let total = ms / 1000;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// 毫秒 → `MM:SS`(不足一小时时更紧凑)。
pub fn format_timestamp_short(ms: u64) -> String {
    let total = ms / 1000;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// 毫秒 → SRT 时间戳 `HH:MM:SS,mmm`。
pub fn format_srt_timestamp(ms: u64) -> String {
    let total = ms;
    let h = total / 3_600_000;
    let m = (total % 3_600_000) / 60_000;
    let s = (total % 60_000) / 1000;
    let msec = total % 1000;
    format!("{h:02}:{m:02}:{s:02},{msec:03}")
}

// ---------------------------------------------------------------------------
// 三种视图
// ---------------------------------------------------------------------------

/// 视图 A:对话体(默认)。
///
/// ```text
/// [00:00:03] 张老师:
/// 今天我们要讲的是神经网络的基本原理……
/// ```
pub fn render_dialogue(segments: &[Segment], labels: &SpeakerLabels) -> String {
    let mut out = String::new();
    for u in merge_for_dialogue_default(segments) {
        let ts = format_timestamp_short(u.start_ms);
        let who = u
            .speaker_id
            .map(|id| labels.display_name(id))
            .unwrap_or_else(|| "—".to_string());
        let mark = if u.overlapped { " ⟂" } else { "" };
        out.push_str(&format!("[{ts}] {who}{mark}:\n{}\n\n", u.text.trim()));
    }
    out.trim_end().to_string()
}

/// 视图 B:时间轴(精确核对)。
///
/// ```text
/// [00:00:03 - 00:00:11]  张老师   今天我们要讲的是
/// ```
pub fn render_timeline(segments: &[Segment], labels: &SpeakerLabels) -> String {
    let mut out = String::new();
    for s in segments {
        if s.text.trim().is_empty() {
            continue;
        }
        let who = s
            .speaker_id
            .map(|id| labels.display_name(id))
            .unwrap_or_else(|| "—".to_string());
        let mark = if s.overlapped { "⟂" } else { " " };
        out.push_str(&format!(
            "[{} - {}] {} {:<6} {}\n",
            format_timestamp(s.start_ms),
            format_timestamp(s.end_ms),
            mark,
            who,
            s.text.trim()
        ));
    }
    out.trim_end().to_string()
}

/// 视图 C:纯文本(喂给 LLM / 复制)。
pub fn render_plain(segments: &[Segment], labels: &SpeakerLabels) -> String {
    let mut out = String::new();
    for u in merge_for_dialogue_default(segments) {
        match u.speaker_id {
            Some(id) => out.push_str(&format!("{}:{}\n", labels.display_name(id), u.text.trim())),
            None => out.push_str(&format!("{}\n", u.text.trim())),
        }
    }
    out.trim_end().to_string()
}

/// 导出 SRT 字幕。
pub fn render_srt(segments: &[Segment], labels: &SpeakerLabels) -> String {
    let mut out = String::new();
    for (i, u) in merge_for_dialogue_default(segments).into_iter().enumerate() {
        let who = u
            .speaker_id
            .map(|id| format!("{}:", labels.display_name(id)))
            .unwrap_or_default();
        out.push_str(&format!(
            "{}\n{} --> {}\n{}{}\n\n",
            i + 1,
            format_srt_timestamp(u.start_ms),
            format_srt_timestamp(u.end_ms),
            who,
            u.text.trim()
        ));
    }
    out.trim_end().to_string()
}

/// 视图枚举,便于 CLI 参数与 UI 下拉共用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewKind {
    Dialogue,
    Timeline,
    Plain,
    Srt,
}

impl ViewKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "dialogue" | "dialog" | "对话" => Some(ViewKind::Dialogue),
            "timeline" | "时间轴" => Some(ViewKind::Timeline),
            "plain" | "text" | "纯文本" => Some(ViewKind::Plain),
            "srt" | "字幕" => Some(ViewKind::Srt),
            _ => None,
        }
    }

    pub fn render(&self, segments: &[Segment], labels: &SpeakerLabels) -> String {
        match self {
            ViewKind::Dialogue => render_dialogue(segments, labels),
            ViewKind::Timeline => render_timeline(segments, labels),
            ViewKind::Plain => render_plain(segments, labels),
            ViewKind::Srt => render_srt(segments, labels),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels_with(ids: &[u32]) -> SpeakerLabels {
        let mut l = SpeakerLabels::new("s");
        l.ensure(ids.iter().copied());
        l.rename(0, "张老师");
        if ids.contains(&1) {
            l.rename(1, "李同学");
        }
        l
    }

    #[test]
    fn timestamp_formats() {
        assert_eq!(format_timestamp(0), "00:00:00");
        assert_eq!(format_timestamp(3_120), "00:00:03");
        assert_eq!(format_timestamp(3_725_000), "01:02:05");
        assert_eq!(format_timestamp_short(3_120), "00:03");
        assert_eq!(format_timestamp_short(3_725_000), "01:02:05");
        assert_eq!(format_srt_timestamp(3_120), "00:00:03,120");
        assert_eq!(format_srt_timestamp(3_725_456), "01:02:05,456");
    }

    #[test]
    fn merge_combines_same_speaker_close_segments() {
        // Whisper 常把一句话切成多段,应合并成一条
        let segs = vec![
            Segment {
                start_ms: 3000,
                end_ms: 6000,
                text: "今天我们要讲的是".into(),
                speaker_id: Some(0),
                overlapped: false,
            },
            Segment {
                start_ms: 6000,
                end_ms: 11_000,
                text: "神经网络的基本原理".into(),
                speaker_id: Some(0),
                overlapped: false,
            },
        ];
        let u = merge_for_dialogue_default(&segs);
        assert_eq!(u.len(), 1, "同一人相邻段落应合并");
        assert_eq!(u[0].text, "今天我们要讲的是神经网络的基本原理");
        assert_eq!(u[0].start_ms, 3000, "时间戳取起始");
        assert_eq!(u[0].end_ms, 11_000);
    }

    #[test]
    fn merge_does_not_cross_speakers() {
        let segs = vec![
            Segment {
                start_ms: 0,
                end_ms: 1000,
                text: "老师说的".into(),
                speaker_id: Some(0),
                overlapped: false,
            },
            Segment {
                start_ms: 1000,
                end_ms: 2000,
                text: "学生说的".into(),
                speaker_id: Some(1),
                overlapped: false,
            },
        ];
        let u = merge_for_dialogue_default(&segs);
        assert_eq!(u.len(), 2, "不同说话人绝不能合并");
        assert_ne!(u[0].speaker_id, u[1].speaker_id);
    }

    #[test]
    fn merge_respects_gap() {
        let segs = vec![
            Segment::new(0, 1000, "第一句"),
            Segment::new(9000, 10_000, "很久以后"),
        ];
        // 默认 max_gap_ms = 2500,间隔 8000ms 不该合并
        let u = merge_for_dialogue_default(&segs);
        assert_eq!(u.len(), 2);
    }

    #[test]
    fn merge_respects_max_chars() {
        let policy = MergePolicy {
            max_chars: 10,
            ..Default::default()
        };
        let segs = vec![
            Segment::new(0, 1000, "一二三四五六"),
            Segment::new(1000, 2000, "七八九十甲乙"),
        ];
        let u = merge_for_dialogue(&segs, &policy);
        assert_eq!(u.len(), 2, "超过字数上限应断开");
    }

    #[test]
    fn merge_respects_max_duration() {
        let policy = MergePolicy {
            max_duration_ms: 5000,
            ..Default::default()
        };
        let segs = vec![
            Segment::new(0, 4000, "前半"),
            Segment::new(4000, 8000, "后半"),
        ];
        let u = merge_for_dialogue(&segs, &policy);
        assert_eq!(u.len(), 2, "超过时长上限应断开");
    }

    #[test]
    fn merge_breaks_after_sentence_end() {
        // 上一段已以句号结束 → 即使间隔很短也不合并,保持句子边界
        let segs = vec![
            Segment::new(0, 1000, "第一句话。"),
            Segment::new(1200, 2000, "第二句话"),
        ];
        let u = merge_for_dialogue_default(&segs);
        assert_eq!(u.len(), 2, "句末标点后应断开");
    }

    #[test]
    fn merge_propagates_overlap_flag() {
        let segs = vec![
            Segment {
                start_ms: 0,
                end_ms: 1000,
                text: "抢话".into(),
                speaker_id: Some(0),
                overlapped: true,
            },
            Segment {
                start_ms: 1000,
                end_ms: 2000,
                text: "继续".into(),
                speaker_id: Some(0),
                overlapped: false,
            },
        ];
        let u = merge_for_dialogue_default(&segs);
        assert_eq!(u.len(), 1);
        assert!(u[0].overlapped, "任一段重叠,合并后应保留标记");
    }

    #[test]
    fn merge_skips_empty_segments() {
        let segs = vec![
            Segment::new(0, 1000, "  "),
            Segment::new(1000, 2000, "有内容"),
        ];
        let u = merge_for_dialogue_default(&segs);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].text, "有内容");
    }

    #[test]
    fn merge_handles_empty_input() {
        assert!(merge_for_dialogue_default(&[]).is_empty());
    }

    #[test]
    fn dialogue_view_has_timestamp_and_speaker() {
        let segs = vec![Segment {
            start_ms: 3120,
            end_ms: 8000,
            text: "今天讲神经网络".into(),
            speaker_id: Some(0),
            overlapped: false,
        }];
        let out = render_dialogue(&segs, &labels_with(&[0, 1]));
        assert!(out.contains("[00:03]"), "{out}");
        assert!(out.contains("张老师:"), "{out}");
        assert!(out.contains("今天讲神经网络"));
    }

    #[test]
    fn dialogue_view_shows_overlap_marker() {
        let segs = vec![Segment {
            start_ms: 0,
            end_ms: 1000,
            text: "抢话内容".into(),
            speaker_id: Some(0),
            overlapped: true,
        }];
        let out = render_dialogue(&segs, &labels_with(&[0]));
        assert!(out.contains('⟂'), "重叠语音应有可见标记: {out}");
    }

    #[test]
    fn timeline_view_shows_range() {
        let segs = vec![Segment {
            start_ms: 3120,
            end_ms: 11_000,
            text: "内容".into(),
            speaker_id: Some(0),
            overlapped: false,
        }];
        let out = render_timeline(&segs, &labels_with(&[0]));
        assert!(out.contains("00:00:03 - 00:00:11"), "{out}");
        assert!(out.contains("张老师"));
    }

    #[test]
    fn plain_view_has_speaker_prefix_no_timestamp() {
        let segs = vec![Segment {
            start_ms: 3120,
            end_ms: 8000,
            text: "内容".into(),
            speaker_id: Some(0),
            overlapped: false,
        }];
        let out = render_plain(&segs, &labels_with(&[0]));
        assert!(out.starts_with("张老师:"), "{out}");
        assert!(!out.contains("00:03"), "纯文本视图不应含时间戳");
    }

    #[test]
    fn plain_view_without_speakers_has_no_prefix() {
        let segs = vec![Segment::new(0, 1000, "没有说话人信息")];
        let out = render_plain(&segs, &SpeakerLabels::new("s"));
        assert_eq!(out, "没有说话人信息");
    }

    #[test]
    fn srt_view_is_wellformed() {
        let segs = vec![
            Segment {
                start_ms: 0,
                end_ms: 2000,
                text: "第一句".into(),
                speaker_id: Some(0),
                overlapped: false,
            },
            Segment {
                start_ms: 2000,
                end_ms: 4000,
                text: "第二句。".into(),
                speaker_id: Some(1),
                overlapped: false,
            },
        ];
        let out = render_srt(&segs, &labels_with(&[0, 1]));
        assert!(out.contains("1\n00:00:00,000 --> 00:00:02,000"), "{out}");
        assert!(out.contains("2\n00:00:02,000 --> 00:00:04,000"), "{out}");
        assert!(out.contains("张老师:第一句"));
    }

    #[test]
    fn view_kind_parse_and_render() {
        assert_eq!(ViewKind::parse("dialogue"), Some(ViewKind::Dialogue));
        assert_eq!(ViewKind::parse("时间轴"), Some(ViewKind::Timeline));
        assert_eq!(ViewKind::parse("srt"), Some(ViewKind::Srt));
        assert_eq!(ViewKind::parse("nope"), None);

        let segs = vec![Segment::new(0, 1000, "x")];
        let l = SpeakerLabels::new("s");
        assert!(!ViewKind::Dialogue.render(&segs, &l).is_empty());
        assert!(!ViewKind::Srt.render(&segs, &l).is_empty());
    }

    #[test]
    fn view_uses_current_display_name_after_rename() {
        // ★ 改名必须立刻反映在所有视图里(ID 引用而非名字引用)
        let segs = vec![Segment {
            start_ms: 0,
            end_ms: 1000,
            text: "内容".into(),
            speaker_id: Some(0),
            overlapped: false,
        }];
        let mut l = SpeakerLabels::new("s");
        l.ensure([0]);
        assert!(render_plain(&segs, &l).starts_with("说话人 1"));
        l.rename(0, "张老师");
        assert!(render_plain(&segs, &l).starts_with("张老师"));
    }

    #[test]
    fn missing_speaker_id_renders_placeholder() {
        let segs = vec![Segment {
            start_ms: 0,
            end_ms: 1000,
            text: "x".into(),
            speaker_id: None,
            overlapped: false,
        }];
        let out = render_dialogue(&segs, &SpeakerLabels::new("s"));
        assert!(out.contains("—:"), "{out}");
    }
}
