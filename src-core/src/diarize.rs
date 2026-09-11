//! 说话人区分(Diarization)。
//!
//! 见技术方案 §7。两阶段设计是这里最重要的结构决定:
//!
//! ```text
//! ① extract_embeddings  —— VAD + 声纹嵌入提取。慢,结果缓存到磁盘
//! ② cluster             —— 用已有嵌入聚类。快(秒级),可反复重跑
//! ```
//!
//! 拆开之后,用户改说话人数量 → 点"重新分离" → 几秒钟出结果 → 不对再改。
//! **试错成本几乎为零,这比"一次跑对"更重要** —— diarization 本来就不存在"一次跑对"。
//!
//! 后端口径与转写解耦:转写可能走 CUDA,diarization 走 CPU,两者各自独立降级。

use crate::types::{Backend, DiarizeMode, Segment, SpeakerStat};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// 说话人未定时的占位 ID。
pub const UNKNOWN_SPEAKER: u32 = u32::MAX;

// ---------------------------------------------------------------------------
// 嵌入数据结构
// ---------------------------------------------------------------------------

/// 一段语音的声纹嵌入,连同它在时间轴上的位置。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Embedding {
    pub start_ms: u64,
    pub end_ms: u64,
    /// L2 归一化后的向量 —— 归一化之后余弦相似度退化成点积,数值更稳。
    pub vector: Vec<f32>,
    /// 质量分(时长 × 电平),用于登记门槛与加权。
    pub quality: f32,
}

impl Embedding {
    pub fn duration_ms(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }

    /// L2 归一化(原地)。
    pub fn normalize(&mut self) {
        let norm: f32 = self.vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > f32::EPSILON {
            for v in self.vector.iter_mut() {
                *v /= norm;
            }
        }
    }
}

/// 一次会话提取出的全部声纹嵌入。这是缓存单位,也是重聚类的输入。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbeddingSet {
    pub session_id: String,
    pub model: String,
    pub backend: Backend,
    pub dimension: usize,
    pub items: Vec<Embedding>,
}

impl EmbeddingSet {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// 按说话人汇总时长与段数,供 UI 展示"谁在主导"。
    pub fn stats(&self, assignments: &[u32]) -> Vec<SpeakerStat> {
        use std::collections::BTreeMap;
        let mut acc: BTreeMap<u32, (u64, u32)> = BTreeMap::new();
        for (e, spk) in self.items.iter().zip(assignments.iter()) {
            let slot = acc.entry(*spk).or_insert((0, 0));
            slot.0 += e.duration_ms();
            slot.1 += 1;
        }
        acc.into_iter()
            .map(|(speaker_id, (talk_time_ms, segment_count))| SpeakerStat {
                speaker_id,
                talk_time_ms,
                segment_count,
            })
            .collect()
    }
}

/// 一个说话人占据的时间区间。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerInterval {
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker_id: u32,
}

// ---------------------------------------------------------------------------
// 余弦相似度与聚类
// ---------------------------------------------------------------------------

/// 余弦相似度。输入应已 L2 归一化(此时等价于点积)。
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        return 0.0;
    }
    (dot / (na * nb)).clamp(-1.0, 1.0)
}

/// 加权中心向量。见技术方案 §8.3。
///
/// 步骤:① 各样本先 L2 归一化 ② 按 (时长 × 质量) 加权平均
/// ③ 只用质量最高的 top_k 个 ④ 中心再归一化
pub fn weighted_centroid(samples: &[Embedding], top_k: usize) -> Option<Vec<f32>> {
    if samples.is_empty() {
        return None;
    }
    let dim = samples[0].vector.len();
    if dim == 0 {
        return None;
    }

    // ③ 取质量最高的 top_k
    let mut idx: Vec<usize> = (0..samples.len()).collect();
    idx.sort_by(|&a, &b| {
        samples[b]
            .quality
            .partial_cmp(&samples[a].quality)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let keep = if top_k == 0 {
        idx.len()
    } else {
        top_k.min(idx.len())
    };
    let idx = &idx[..keep];

    // ② 加权平均(样本已归一化,见 Embedding::normalize)
    let mut acc = vec![0.0f32; dim];
    let mut total_w = 0.0f32;
    for &i in idx {
        let s = &samples[i];
        if s.vector.len() != dim {
            continue;
        }
        // 权重:质量优先,且给时长一个温和的增益,避免超长录音独占
        let dur_s = (s.duration_ms() as f32 / 1000.0).clamp(0.5, 60.0);
        let w = s.quality.max(1e-3) * dur_s.sqrt();
        for (a, v) in acc.iter_mut().zip(s.vector.iter()) {
            *a += v * w;
        }
        total_w += w;
    }
    if total_w <= f32::EPSILON {
        return None;
    }
    for a in acc.iter_mut() {
        *a /= total_w;
    }

    // ④ 中心再归一化
    let norm: f32 = acc.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for a in acc.iter_mut() {
            *a /= norm;
        }
    }
    Some(acc)
}

/// 聚类结果。
#[derive(Clone, Debug)]
pub struct ClusterResult {
    /// 与 items 等长,每个嵌入归属的 speaker_id
    pub assignments: Vec<u32>,
    pub num_clusters: u32,
}

/// 对嵌入做聚类。
///
/// - `Fixed(n)`:固定 k 的 k-means(封闭集求解,快且不会过分割)
/// - `Auto` / `Range`:先用距离阈值做凝聚聚类,再按范围裁剪
///
/// 见技术方案 §7.2 —— 指定人数之所以简单得多,就是因为这里从"搜索"变成"求解"。
pub fn cluster(embeddings: &EmbeddingSet, mode: DiarizeMode) -> Result<ClusterResult> {
    let n = embeddings.items.len();
    if n == 0 {
        return Ok(ClusterResult {
            assignments: vec![],
            num_clusters: 0,
        });
    }
    if n == 1 {
        return Ok(ClusterResult {
            assignments: vec![0],
            num_clusters: 1,
        });
    }

    match mode {
        DiarizeMode::Fixed(k) => kmeans(&embeddings.items, k.max(1) as usize),
        DiarizeMode::Auto => {
            // 阈值凝聚聚类,典型课堂 2~8 人
            let r = agglomerative(&embeddings.items, 0.62, 2, 8)?;
            Ok(r)
        }
        DiarizeMode::Range(lo, hi) => {
            let r = agglomerative(&embeddings.items, 0.62, lo.max(1), hi.max(lo))?;
            Ok(r)
        }
    }
}

/// 固定 k 的 k-means(余弦空间,输入已归一化)。
fn kmeans(items: &[Embedding], k: usize) -> Result<ClusterResult> {
    let n = items.len();
    let dim = items[0].vector.len();
    if dim == 0 {
        return Err(anyhow!("嵌入维度为 0"));
    }
    let k = k.min(n).max(1);

    // k-means++ 初始化
    let mut centers: Vec<Vec<f32>> = Vec::with_capacity(k);
    centers.push(items[0].vector.clone());
    while centers.len() < k {
        let dists: Vec<f32> = items
            .iter()
            .map(|it| {
                let best = centers
                    .iter()
                    .map(|c| 1.0 - cosine_similarity(&it.vector, c))
                    .fold(f32::MAX, f32::min);
                best.max(0.0).powi(2)
            })
            .collect();
        let total: f32 = dists.iter().sum();
        let pick = if total <= f32::EPSILON {
            centers.len() % n
        } else {
            // 确定性选择:取距离最大的点(不引入随机数,保证可复现)
            dists
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(i, _)| i)
                .unwrap_or(0)
        };
        centers.push(items[pick].vector.clone());
    }

    let mut assignments = vec![0u32; n];
    for _ in 0..25 {
        let mut changed = false;
        for (i, it) in items.iter().enumerate() {
            let mut best = 0usize;
            let mut best_sim = f32::MIN;
            for (ci, c) in centers.iter().enumerate() {
                let s = cosine_similarity(&it.vector, c);
                if s > best_sim {
                    best_sim = s;
                    best = ci;
                }
            }
            if assignments[i] as usize != best {
                assignments[i] = best as u32;
                changed = true;
            }
        }
        // 更新中心
        let mut sums = vec![vec![0.0f32; dim]; k];
        let mut counts = vec![0usize; k];
        for (i, it) in items.iter().enumerate() {
            let c = assignments[i] as usize;
            for (a, v) in sums[c].iter_mut().zip(it.vector.iter()) {
                *a += v;
            }
            counts[c] += 1;
        }
        for c in 0..k {
            if counts[c] == 0 {
                // 空簇:重新播种到离现有中心最远的点,避免退化
                let far = items
                    .iter()
                    .enumerate()
                    .max_by(|(_, x), (_, y)| {
                        let dx = centers
                            .iter()
                            .map(|c| cosine_similarity(&x.vector, c))
                            .fold(f32::MIN, f32::max);
                        let dy = centers
                            .iter()
                            .map(|c| cosine_similarity(&y.vector, c))
                            .fold(f32::MIN, f32::max);
                        dx.partial_cmp(&dy).unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                centers[c] = items[far].vector.clone();
                continue;
            }
            let norm: f32 = sums[c].iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > f32::EPSILON {
                centers[c] = sums[c].iter().map(|x| x / norm).collect();
            }
        }
        if !changed {
            break;
        }
    }

    // 压缩 ID,去掉空簇造成的空洞
    let remapped = compact_ids(&assignments);
    Ok(ClusterResult {
        num_clusters: remapped.iter().copied().max().map(|m| m + 1).unwrap_or(0),
        assignments: remapped,
    })
}

/// 阈值凝聚聚类。用于不知道人数时的估计(给用户一个可修改的默认值)。
fn agglomerative(
    items: &[Embedding],
    threshold: f32,
    min_k: u8,
    max_k: u8,
) -> Result<ClusterResult> {
    let n = items.len();
    let mut clusters: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();

    loop {
        if clusters.len() <= min_k as usize {
            break;
        }
        // 找最相似的一对
        let mut best: Option<(usize, usize, f32)> = None;
        for i in 0..clusters.len() {
            for j in (i + 1)..clusters.len() {
                let ci = centroid_of(items, &clusters[i]);
                let cj = centroid_of(items, &clusters[j]);
                let sim = cosine_similarity(&ci, &cj);
                if best.map(|(_, _, s)| sim > s).unwrap_or(true) {
                    best = Some((i, j, sim));
                }
            }
        }
        match best {
            Some((i, j, sim)) if sim >= threshold => {
                let b = clusters.remove(j);
                clusters[i].extend(b);
            }
            _ => break,
        }
    }

    // 超过上限就继续合并最相似的
    while clusters.len() > max_k as usize {
        let mut best: Option<(usize, usize, f32)> = None;
        for i in 0..clusters.len() {
            for j in (i + 1)..clusters.len() {
                let sim = cosine_similarity(
                    &centroid_of(items, &clusters[i]),
                    &centroid_of(items, &clusters[j]),
                );
                if best.map(|(_, _, s)| sim > s).unwrap_or(true) {
                    best = Some((i, j, sim));
                }
            }
        }
        match best {
            Some((i, j, _)) => {
                let b = clusters.remove(j);
                clusters[i].extend(b);
            }
            None => break,
        }
    }

    let mut assignments = vec![0u32; n];
    for (cid, members) in clusters.iter().enumerate() {
        for &m in members {
            assignments[m] = cid as u32;
        }
    }
    let assignments = compact_ids(&assignments);
    Ok(ClusterResult {
        num_clusters: assignments.iter().copied().max().map(|m| m + 1).unwrap_or(0),
        assignments,
    })
}

fn centroid_of(items: &[Embedding], members: &[usize]) -> Vec<f32> {
    if members.is_empty() {
        return vec![];
    }
    let dim = items[members[0]].vector.len();
    let mut acc = vec![0.0f32; dim];
    for &m in members {
        for (a, v) in acc.iter_mut().zip(items[m].vector.iter()) {
            *a += v;
        }
    }
    let norm: f32 = acc.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for a in acc.iter_mut() {
            *a /= norm;
        }
    }
    acc
}

/// 把 ID 压缩成 0..k 连续编号,并按出现顺序排序。
fn compact_ids(assignments: &[u32]) -> Vec<u32> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<u32, u32> = BTreeMap::new();
    let mut next = 0u32;
    assignments
        .iter()
        .map(|a| {
            *map.entry(*a).or_insert_with(|| {
                let v = next;
                next += 1;
                v
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 段落级重叠投票(避开 CJK 字级时间戳的坑)
// ---------------------------------------------------------------------------

/// 把说话人区间分配给转写段落。
///
/// ⚠️ `whisper.cpp` 的 `--max-len 1`(字级时间戳)**在 CJK 上工作得很差**,
/// 而字级时间戳正是对齐的标准做法 —— 这条路对中文不通。
/// 改用段落级重叠投票:不受时间戳粒度限制、天然能标记重叠语音。
///
/// 返回 (speaker_id, overlapped)。`overlapped` 在前两名比例接近时为 true ——
/// 这不是噪音,而是有用的信号:它告诉用户"这句可能是抢话"。
pub fn assign_speaker(seg: &Segment, intervals: &[SpeakerInterval]) -> (u32, bool) {
    use std::collections::BTreeMap;
    let mut by_speaker: BTreeMap<u32, u64> = BTreeMap::new();
    for iv in intervals {
        let ov = overlap_ms(seg.start_ms, seg.end_ms, iv.start_ms, iv.end_ms);
        if ov > 0 {
            *by_speaker.entry(iv.speaker_id).or_insert(0) += ov;
        }
    }
    if by_speaker.is_empty() {
        return (UNKNOWN_SPEAKER, false);
    }

    let mut ranked: Vec<(u32, u64)> = by_speaker.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let (top_id, top_ov) = ranked[0];
    let overlapped =
        ranked.len() > 1 && top_ov > 0 && (ranked[1].1 as f32 / top_ov as f32) > 0.75;
    (top_id, overlapped)
}

fn overlap_ms(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> u64 {
    a_end.min(b_end).saturating_sub(a_start.max(b_start))
}

/// 把聚类结果铺成连续的时间区间(供 assign_speaker 使用)。
pub fn to_intervals(embeddings: &EmbeddingSet, assignments: &[u32]) -> Vec<SpeakerInterval> {
    embeddings
        .items
        .iter()
        .zip(assignments.iter())
        .map(|(e, s)| SpeakerInterval {
            start_ms: e.start_ms,
            end_ms: e.end_ms,
            speaker_id: *s,
        })
        .collect()
}

/// 一站式:给转写段落标注说话人。
pub fn annotate_segments(segments: &mut [Segment], intervals: &[SpeakerInterval]) {
    for seg in segments.iter_mut() {
        let (spk, ov) = assign_speaker(seg, intervals);
        seg.speaker_id = if spk == UNKNOWN_SPEAKER { None } else { Some(spk) };
        seg.overlapped = ov;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emb(v: Vec<f32>, start: u64, end: u64) -> Embedding {
        let mut e = Embedding {
            start_ms: start,
            end_ms: end,
            vector: v,
            quality: 1.0,
        };
        e.normalize();
        e
    }

    fn set(items: Vec<Embedding>) -> EmbeddingSet {
        let dim = items.first().map(|e| e.vector.len()).unwrap_or(0);
        EmbeddingSet {
            session_id: "s".into(),
            model: "m".into(),
            backend: Backend::Cpu,
            dimension: dim,
            items,
        }
    }

    #[test]
    fn cosine_identical_is_one() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!((cosine_similarity(&[1.0, 0.0], &[0.0, 1.0])).abs() < 1e-6);
        assert!((cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_handles_degenerate_input() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), 0.0);
    }

    #[test]
    fn weighted_centroid_averages_similar_samples() {
        let s = vec![
            emb(vec![1.0, 0.0, 0.0], 0, 1000),
            emb(vec![0.98, 0.02, 0.0], 1000, 2000),
        ];
        let c = weighted_centroid(&s, 8).unwrap();
        assert!(c[0] > 0.99, "中心应靠近主方向: {c:?}");
    }

    #[test]
    fn weighted_centroid_top_k_limits_influence() {
        // 一个高质量正样本 + 一堆低质量反向样本
        let mut good = emb(vec![1.0, 0.0], 0, 10_000);
        good.quality = 10.0;
        let mut bad = emb(vec![-1.0, 0.0], 0, 500);
        bad.quality = 0.01;
        let many_bad: Vec<Embedding> = (0..5).map(|_| bad.clone()).collect();
        let mut all = vec![good.clone()];
        all.extend(many_bad);
        let c = weighted_centroid(&all, 1).unwrap();
        // 只保留质量最高的 1 个 → 不被坏样本拖偏
        assert!(c[0] > 0.9, "top_k=1 应只采信高质量样本: {c:?}");
    }

    #[test]
    fn weighted_centroid_empty_is_none() {
        assert!(weighted_centroid(&[], 8).is_none());
    }

    #[test]
    fn kmeans_separates_two_clear_groups() {
        // 两组正交方向,各 4 条
        let mut items = vec![];
        for i in 0..4 {
            items.push(emb(vec![1.0, 0.0, 0.0], i * 1000, i * 1000 + 900));
        }
        for i in 0..4 {
            items.push(emb(vec![0.0, 1.0, 0.0], 10_000 + i * 1000, 10_000 + i * 1000 + 900));
        }
        let es = set(items);
        let r = cluster(&es, DiarizeMode::Fixed(2)).unwrap();
        assert_eq!(r.num_clusters, 2);
        // 前四条同一簇、后四条另一簇
        assert!(r.assignments[0..4].iter().all(|x| *x == r.assignments[0]));
        assert!(r.assignments[4..8].iter().all(|x| *x == r.assignments[4]));
        assert_ne!(r.assignments[0], r.assignments[4]);
    }

    #[test]
    fn fixed_count_never_exceeds_requested() {
        let mut items = vec![];
        for i in 0..8 {
            let v = if i % 2 == 0 {
                vec![1.0, 0.0]
            } else {
                vec![0.0, 1.0]
            };
            items.push(emb(v, i as u64 * 1000, i as u64 * 1000 + 900));
        }
        let es = set(items);
        // 强制要 5 类,但数据只有 2 类 → 不应超过 5,且能正常返回
        let r = cluster(&es, DiarizeMode::Fixed(5)).unwrap();
        assert!(r.num_clusters <= 5);
        assert_eq!(r.assignments.len(), 8);
    }

    #[test]
    fn clustering_is_deterministic() {
        let mut items = vec![];
        for i in 0..10 {
            let v = vec![(i % 3) as f32 + 1.0, ((i + 1) % 3) as f32, 0.5];
            items.push(emb(v, i as u64 * 1000, i as u64 * 1000 + 900));
        }
        let es = set(items);
        let a = cluster(&es, DiarizeMode::Fixed(3)).unwrap();
        let b = cluster(&es, DiarizeMode::Fixed(3)).unwrap();
        assert_eq!(a.assignments, b.assignments, "聚类必须可复现");
    }

    #[test]
    fn auto_mode_respects_bounds() {
        let mut items = vec![];
        for i in 0..30 {
            // 三个明显方向 + 噪声
            let mut v = vec![0.0f32; 3];
            v[i % 3] = 1.0;
            v[(i + 1) % 3] = 0.05 * (i as f32 % 2.0);
            items.push(emb(v, i as u64 * 1000, i as u64 * 1000 + 900));
        }
        let es = set(items);
        let r = cluster(&es, DiarizeMode::Auto).unwrap();
        assert!(r.num_clusters >= 2 && r.num_clusters <= 8, "得到 {}", r.num_clusters);
    }

    #[test]
    fn cluster_handles_empty_and_single() {
        let es = set(vec![]);
        assert_eq!(cluster(&es, DiarizeMode::Auto).unwrap().num_clusters, 0);
        let es1 = set(vec![emb(vec![1.0, 0.0], 0, 1000)]);
        assert_eq!(cluster(&es1, DiarizeMode::Fixed(4)).unwrap().num_clusters, 1);
    }

    #[test]
    fn assign_speaker_picks_longest_overlap() {
        let seg = Segment::new(1000, 5000, "hello");
        let ivs = vec![
            SpeakerInterval { start_ms: 0, end_ms: 2000, speaker_id: 7 },
            SpeakerInterval { start_ms: 2000, end_ms: 5000, speaker_id: 3 },
        ];
        let (spk, ov) = assign_speaker(&seg, &ivs);
        assert_eq!(spk, 3, "应归属重叠更长的说话人");
        assert!(!ov);
    }

    #[test]
    fn assign_speaker_flags_overlap_when_competitive() {
        // 两个说话人几乎平分这段语音 → 应标记为重叠
        let seg = Segment::new(0, 1000, "抢话");
        let ivs = vec![
            SpeakerInterval { start_ms: 0, end_ms: 505, speaker_id: 1 },
            SpeakerInterval { start_ms: 495, end_ms: 1000, speaker_id: 2 },
        ];
        let (_, ov) = assign_speaker(&seg, &ivs);
        assert!(ov, "前两名接近时应标记为重叠/不确定");
    }

    #[test]
    fn assign_speaker_unknown_when_no_overlap() {
        let seg = Segment::new(9000, 9500, "x");
        let ivs = vec![SpeakerInterval { start_ms: 0, end_ms: 1000, speaker_id: 1 }];
        assert_eq!(assign_speaker(&seg, &ivs).0, UNKNOWN_SPEAKER);
    }

    #[test]
    fn annotate_segments_writes_speaker_and_clears_unknown() {
        let mut segs = vec![
            Segment::new(0, 1000, "a"),
            Segment::new(50_000, 51_000, "b"),
        ];
        let ivs = vec![SpeakerInterval { start_ms: 0, end_ms: 2000, speaker_id: 5 }];
        annotate_segments(&mut segs, &ivs);
        assert_eq!(segs[0].speaker_id, Some(5));
        assert_eq!(segs[1].speaker_id, None, "无重叠的段落应留空而不是硬塞");
    }

    #[test]
    fn stats_sums_talk_time() {
        let es = set(vec![
            emb(vec![1.0, 0.0], 0, 2000),
            emb(vec![1.0, 0.0], 2000, 5000),
            emb(vec![0.0, 1.0], 5000, 6000),
        ]);
        let st = es.stats(&[0, 0, 1]);
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].talk_time_ms, 5000);
        assert_eq!(st[0].segment_count, 2);
        assert_eq!(st[1].talk_time_ms, 1000);
    }

    #[test]
    fn compact_ids_has_no_holes() {
        let c = compact_ids(&[5, 5, 9, 2, 9]);
        assert_eq!(*c.iter().max().unwrap(), 2);
    }

    #[test]
    fn normalize_is_idempotent() {
        let mut e = Embedding {
            start_ms: 0,
            end_ms: 1,
            vector: vec![3.0, 4.0],
            quality: 1.0,
        };
        e.normalize();
        let first = e.vector.clone();
        e.normalize();
        for (a, b) in first.iter().zip(e.vector.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
        assert!((first[0] - 0.6).abs() < 1e-6);
    }
}
