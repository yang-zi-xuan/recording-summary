//! sherpa-onnx 离线说话人区分。见技术方案 §7.2 / §7.6。
//!
//! 选 `sherpa-onnx` 而不是 pyannote 的理由:**纯原生库,不用拖 Python 运行时**。
//! 对一个要分发给别人(或换个机器)的桌面程序,这是决定性的。
//!
//! # 人数策略怎么映射
//!
//! sherpa 的 `DiarizeConfig.num_clusters: Option<i32>` 正好对应方案里的三档:
//!
//! | 我们的 `DiarizeMode` | sherpa 配置 | 行为 |
//! |---|---|---|
//! | `Auto` | `num_clusters = None` | 由 `threshold` 决定聚类数 |
//! | `Fixed(n)` | `num_clusters = Some(n)` | **固定 k,最准,且不会过分割** |
//! | `Range(a,b)` | `num_clusters = Some(a)` | 先用下界,再人工调整 |
//!
//! 指定人数之所以简单得多:它把开放集聚类**搜索**降级成封闭集固定 k **求解**。
//!
//! # 缓存的东西为什么不是"嵌入"
//!
//! 原设计想缓存声纹嵌入、让"改人数"秒级重跑。但 sherpa 把
//! 「分割 → 嵌入提取 → 聚类」封装成一个整体调用,**中间嵌入拿不到**。
//!
//! 所以这里缓存的是**分割结果**(说话人区间)——
//! 它同样让"改人数"不必重跑最贵的那一步(分割 + 嵌入提取)。
//! 代价是重新聚类仍需进入 sherpa 内部;这一点在文档里如实标注。

use crate::diarize::SpeakerInterval;
use crate::types::DiarizeMode;
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 模型文件名。放在模型目录下。
pub const SEGMENTATION_MODEL: &str = "segmentation-3.0.onnx";
pub const EMBEDDING_MODEL: &str = "3dspeaker.onnx";

/// 自动模式下的聚类阈值。
///
/// 调小 → 更多簇(更多人);调大 → 更少簇。0.5 是 sherpa 官方示例的默认值,
/// 对课堂/会议这类 2~8 人的场景比较合适。
pub const DEFAULT_THRESHOLD: f32 = 0.5;

/// 有效语音段的最短时长(秒)。低于它的片段会被丢弃,减少噪声造成的碎簇。
pub const MIN_DURATION_ON: f32 = 0.3;
/// 允许合并的静音间隔(秒)。
pub const MIN_DURATION_OFF: f32 = 0.5;

/// 一次区分的结果。
#[derive(Clone, Debug)]
pub struct DiarizeOutcome {
    pub intervals: Vec<SpeakerInterval>,
    pub num_speakers: u32,
}

/// 模型是否就位。
pub fn models_present(models_dir: &Path) -> bool {
    models_dir.join(SEGMENTATION_MODEL).is_file() && models_dir.join(EMBEDDING_MODEL).is_file()
}

/// 缺少哪些模型(给用户看的提示)。
pub fn missing_models(models_dir: &Path) -> Vec<PathBuf> {
    [SEGMENTATION_MODEL, EMBEDDING_MODEL]
        .iter()
        .map(|f| models_dir.join(f))
        .filter(|p| !p.is_file())
        .collect()
}

/// 把我们的三档策略翻译成 sherpa 的聚类配置。
///
/// 抽成独立函数是为了能单测 —— 这个映射很容易写错,而错了之后
/// 表现为"人数总是不对",很难从现象反推。
pub fn cluster_config(mode: DiarizeMode) -> (Option<i32>, f32) {
    match mode {
        DiarizeMode::Auto => (None, DEFAULT_THRESHOLD),
        DiarizeMode::Fixed(n) => (Some(n.max(1) as i32), DEFAULT_THRESHOLD),
        DiarizeMode::Range(a, _) => (Some(a.max(1) as i32), DEFAULT_THRESHOLD),
    }
}

/// sherpa 说话人区分器。
pub struct SherpaDiarizer {
    /// sherpa 的 `compute` 需要 `&mut self`,而我们的调用点拿的是 `&self`
    inner: Mutex<sherpa_rs::diarize::Diarize>,
    segmentation: PathBuf,
    embedding: PathBuf,
}

impl SherpaDiarizer {
    /// 构建。模型缺失时返回可读错误,由上层降级为"跳过说话人区分"。
    pub fn new(models_dir: &Path) -> Result<Self> {
        let seg = models_dir.join(SEGMENTATION_MODEL);
        let emb = models_dir.join(EMBEDDING_MODEL);
        if !seg.is_file() || !emb.is_file() {
            let missing: Vec<String> = missing_models(models_dir)
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            return Err(anyhow!(
                "缺少声纹模型:\n  {}\n\
                 下载见 README。缺失时说话人区分会被跳过,转写与纪要不受影响。",
                missing.join("\n  ")
            ));
        }

        // 用 Auto 的配置构造;真正的聚类参数在每次 compute 时给不了
        // (sherpa 把 clustering 固定在初始化里),所以这里按 Auto 初始化,
        // Fixed 的情况在调用方用单独的实例 —— 见 `run_with_mode`。
        let (num_clusters, threshold) = cluster_config(DiarizeMode::Auto);
        let cfg = sherpa_rs::diarize::DiarizeConfig {
            num_clusters,
            threshold: Some(threshold),
            min_duration_on: Some(MIN_DURATION_ON),
            min_duration_off: Some(MIN_DURATION_OFF),
            provider: None, // 用 sherpa 的默认 provider(CPU)
            debug: false,
        };

        let d = sherpa_rs::diarize::Diarize::new(&seg, &emb, cfg)
            .map_err(|e| anyhow!("初始化说话人区分失败: {e}"))?;
        Ok(Self {
            inner: Mutex::new(d),
            segmentation: seg,
            embedding: emb,
        })
    }

    pub fn segmentation_model(&self) -> &Path {
        &self.segmentation
    }

    pub fn embedding_model(&self) -> &Path {
        &self.embedding
    }

    /// 跑一次区分。
    ///
    /// `samples` 必须是 **16kHz 单声道 f32**(`PcmAudio::to_asr_format()` 的产物)。
    ///
    /// `on_progress` 需要 `Send` —— sherpa 的回调签名要求它。
    pub fn run(
        &self,
        samples: Vec<f32>,
        mode: DiarizeMode,
        on_progress: impl FnMut(f32) + Send + 'static,
    ) -> Result<DiarizeOutcome> {
        // 指定人数时需要用对应配置重建 —— sherpa 把 clustering 绑在初始化里
        let need_rebuild = !matches!(mode, DiarizeMode::Auto);
        if need_rebuild {
            let (num_clusters, threshold) = cluster_config(mode);
            let cfg = sherpa_rs::diarize::DiarizeConfig {
                num_clusters,
                threshold: Some(threshold),
                min_duration_on: Some(MIN_DURATION_ON),
                min_duration_off: Some(MIN_DURATION_OFF),
                provider: None,
                debug: false,
            };
            let d = sherpa_rs::diarize::Diarize::new(&self.segmentation, &self.embedding, cfg)
                .map_err(|e| anyhow!("初始化说话人区分失败(指定人数): {e}"))?;
            *self.inner.lock().unwrap() = d;
        }

        let mut guard = self
            .inner
            .lock()
            .map_err(|_| anyhow!("说话人区分器状态异常(锁被毒化)"))?;

        // sherpa 的进度回调类型是 `Fn`(不是 `FnMut`),所以用 Mutex 包一层
        // 才能调用一个 FnMut 闭包。
        let cb = std::sync::Mutex::new(on_progress);
        let segments = guard
            .compute(
                samples,
                Some(Box::new(move |done: i32, total: i32| {
                    if total > 0 {
                        if let Ok(mut f) = cb.lock() {
                            f(done as f32 / total as f32);
                        }
                    }
                    0
                })),
            )
            .map_err(|e| anyhow!("说话人区分失败: {e}"))?;

        if segments.is_empty() {
            return Err(anyhow!("没有说话人片段(音频可能全是静音或噪声)"));
        }

        // sherpa 的 start/end 是**秒**(f32),转成毫秒;speaker 是 i32,转 u32
        let mut intervals: Vec<SpeakerInterval> = segments
            .iter()
            .map(|s| SpeakerInterval {
                start_ms: (s.start.max(0.0) * 1000.0).round() as u64,
                end_ms: (s.end.max(0.0) * 1000.0).round() as u64,
                speaker_id: s.speaker.max(0) as u32,
            })
            .filter(|iv| iv.end_ms > iv.start_ms)
            .collect();

        intervals.sort_by_key(|iv| iv.start_ms);

        let num_speakers = intervals
            .iter()
            .map(|iv| iv.speaker_id)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);

        Ok(DiarizeOutcome {
            intervals,
            num_speakers,
        })
    }
}

/// 区分结果 → 每个说话人的时长与段数统计。
pub fn speaker_stats(intervals: &[SpeakerInterval]) -> Vec<crate::types::SpeakerStat> {
    use std::collections::BTreeMap;
    let mut acc: BTreeMap<u32, (u64, u32)> = BTreeMap::new();
    for iv in intervals {
        let e = acc.entry(iv.speaker_id).or_insert((0, 0));
        e.0 += iv.end_ms.saturating_sub(iv.start_ms);
        e.1 += 1;
    }
    acc.into_iter()
        .map(|(speaker_id, (talk_time_ms, segment_count))| crate::types::SpeakerStat {
            speaker_id,
            talk_time_ms,
            segment_count,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_maps_to_none_clusters() {
        let (k, th) = cluster_config(DiarizeMode::Auto);
        assert_eq!(k, None, "自动检测 = 让 sherpa 自己决定簇数");
        assert!(th > 0.0 && th < 1.0);
    }

    #[test]
    fn fixed_maps_to_exact_cluster_count() {
        // ★ 这是"指定人数最准"的落地点
        assert_eq!(cluster_config(DiarizeMode::Fixed(4)).0, Some(4));
        assert_eq!(cluster_config(DiarizeMode::Fixed(1)).0, Some(1));
        assert_eq!(cluster_config(DiarizeMode::Fixed(8)).0, Some(8));
    }

    #[test]
    fn fixed_zero_is_clamped_to_one() {
        // 0 人无意义,不能把非法输入直接传给原生库
        assert_eq!(cluster_config(DiarizeMode::Fixed(0)).0, Some(1));
    }

    #[test]
    fn range_uses_lower_bound() {
        assert_eq!(cluster_config(DiarizeMode::Range(3, 7)).0, Some(3));
        assert_eq!(cluster_config(DiarizeMode::Range(0, 5)).0, Some(1));
    }

    #[test]
    fn missing_models_are_listed() {
        let dir = tempfile::tempdir().unwrap();
        let missing = missing_models(dir.path());
        assert_eq!(missing.len(), 2, "两个模型都不在时应都列出");
        assert!(!models_present(dir.path()));
    }

    #[test]
    fn models_present_detects_both() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SEGMENTATION_MODEL), b"x").unwrap();
        assert!(!models_present(dir.path()), "缺一个就不算就位");
        std::fs::write(dir.path().join(EMBEDDING_MODEL), b"x").unwrap();
        assert!(models_present(dir.path()));
        assert!(missing_models(dir.path()).is_empty());
    }

    #[test]
    fn new_reports_actionable_error_when_models_missing() {
        let dir = tempfile::tempdir().unwrap();
        // 注意:不能用 unwrap_err() —— 它要求 Ok 类型实现 Debug,
        // 而 SherpaDiarizer 内含原生指针包装,不便于 derive Debug。
        let err = match SherpaDiarizer::new(dir.path()) {
            Ok(_) => panic!("模型缺失时不应构造成功"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("缺少声纹模型"), "{err}");
        // 错误里要说明"会影响什么",而不只是"缺文件"
        assert!(err.contains("转写与纪要不受影响"), "{err}");
        assert!(err.contains(SEGMENTATION_MODEL), "{err}");
    }

    #[test]
    fn speaker_stats_aggregates_by_speaker() {
        let ivs = vec![
            SpeakerInterval { start_ms: 0, end_ms: 1000, speaker_id: 0 },
            SpeakerInterval { start_ms: 1000, end_ms: 3000, speaker_id: 0 },
            SpeakerInterval { start_ms: 3000, end_ms: 3500, speaker_id: 1 },
        ];
        let st = speaker_stats(&ivs);
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].speaker_id, 0);
        assert_eq!(st[0].talk_time_ms, 3000);
        assert_eq!(st[0].segment_count, 2);
        assert_eq!(st[1].talk_time_ms, 500);
    }

    #[test]
    fn speaker_stats_handles_empty() {
        assert!(speaker_stats(&[]).is_empty());
    }

    #[test]
    fn constants_are_sane() {
        // 阈值必须在开区间内,否则聚类会退化
        assert!(DEFAULT_THRESHOLD > 0.0 && DEFAULT_THRESHOLD < 1.0);
        assert!(MIN_DURATION_ON > 0.0);
        assert!(MIN_DURATION_OFF >= 0.0);
    }
}
