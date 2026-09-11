//! 声纹档案(跨会话音色辨认)。见技术方案 §8。
//!
//! 补的是纯 diarization 最大的短板:**它是"会话内"的** ——
//! 录 10 次课,它 10 次都从零开始猜。有了档案才能"越用越准"。
//!
//! 三个不能省的设计(它们是功能,不是防护):
//!
//! 1. **档案 = 加权中心,不是单个向量** —— 单个向量对信道差异太脆,
//!    换个麦克风差异可能比"不同人"还大。
//! 2. **三级匹配** —— 猜错且不可见是灾难;中间档让错误可见。
//! 3. **保留原始样本** —— 中心点必须可重算,否则算法一改旧数据就废。

use crate::diarize::{cosine_similarity, weighted_centroid, Embedding};
use crate::store::db::{now_ms, Db};
use crate::store::files::{FileStore, ProfileEntry};
use anyhow::{anyhow, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

/// 自动标注的相似度门槛。
pub const THRESHOLD_CONFIRMED: f32 = 0.72;
/// 提示"可能是 X"的门槛。
pub const THRESHOLD_UNCERTAIN: f32 = 0.55;

/// 登记样本的最短时长(毫秒)。太短的样本对中枢贡献是负的。
pub const MIN_SAMPLE_MS: u64 = 3000;

/// 参与计算中心的样本数上限。防止一次超长录音主导档案。
pub const TOP_K_SAMPLES: usize = 8;

// ---------------------------------------------------------------------------
// 类型
// ---------------------------------------------------------------------------

/// 匹配置信度。**三级是核心设计** —— 中间档让不可见的错误变得可见。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchConfidence {
    /// 高置信度,可自动标注
    Confirmed,
    /// 中置信度,UI 显示"可能是 X?"等用户确认
    Uncertain,
    /// 未匹配,新建说话人
    New,
}

impl MatchConfidence {
    pub fn from_similarity(sim: f32) -> Self {
        if sim >= THRESHOLD_CONFIRMED {
            MatchConfidence::Confirmed
        } else if sim >= THRESHOLD_UNCERTAIN {
            MatchConfidence::Uncertain
        } else {
            MatchConfidence::New
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            MatchConfidence::Confirmed => "已确认",
            MatchConfidence::Uncertain => "待确认",
            MatchConfidence::New => "新说话人",
        }
    }
}

/// 一次匹配结果。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchResult {
    pub speaker_id: u32,
    pub profile_id: Option<String>,
    pub display_name: Option<String>,
    pub similarity: f32,
    pub confidence: MatchConfidence,
}

/// 一条登记样本。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnrollSample {
    pub session_id: String,
    pub embedding: Vec<f32>,
    pub start_ms: u64,
    pub end_ms: u64,
    /// 质量分(时长 × 电平)
    pub quality: f32,
}

impl EnrollSample {
    pub fn duration_ms(&self) -> u64 {
        self.end_ms.saturating_sub(self.start_ms)
    }

    /// 样本是否够格入库。
    pub fn is_acceptable(&self) -> bool {
        self.duration_ms() >= MIN_SAMPLE_MS && self.quality > 0.0 && !self.embedding.is_empty()
    }
}

/// 档案(内存视图)。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpeakerProfile {
    pub profile_id: String,
    pub display_name: String,
    pub centroid: Vec<f32>,
    pub embedding_model: String,
    pub sample_count: u32,
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------
// 档案管理
// ---------------------------------------------------------------------------

pub struct VoiceprintStore<'a> {
    db: &'a Db,
    files: &'a FileStore,
}

impl<'a> VoiceprintStore<'a> {
    pub fn new(db: &'a Db, files: &'a FileStore) -> Self {
        Self { db, files }
    }

    /// 载入全部档案。`embedding_model` 不匹配的档案会被跳过并告警 ——
    /// 换声纹模型 = 向量空间变了,旧档案全部失效。
    pub fn load_profiles(&self, embedding_model: &str) -> Result<Vec<SpeakerProfile>> {
        let mut stmt = self.db.conn().prepare(
            "SELECT profile_id, display_name, centroid, embedding_model, sample_count, note
             FROM speaker_profiles",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<Vec<u8>>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;

        let mut out = Vec::new();
        let mut stale = 0usize;
        for row in rows {
            let (id, name, centroid_blob, model, count, note) = row?;
            if model != embedding_model {
                stale += 1;
                tracing::warn!(
                    "档案 {id}({name})是用 {model} 生成的,当前模型为 {embedding_model};已跳过。\
                     换声纹模型后旧档案无法使用,需要重新登记。"
                );
                continue;
            }
            let centroid: Vec<f32> = centroid_blob
                .as_deref()
                .map(serde_json::from_slice)
                .transpose()?
                .unwrap_or_default();
            if centroid.is_empty() {
                continue;
            }
            out.push(SpeakerProfile {
                profile_id: id,
                display_name: name,
                centroid,
                embedding_model: model,
                sample_count: count as u32,
                note,
            });
        }
        if stale > 0 {
            tracing::warn!("有 {stale} 个档案因模型不匹配被跳过");
        }
        Ok(out)
    }

    /// 登记:把某会话里某个说话人的样本加进档案(可新建)。
    ///
    /// ★ **只登记用户确认过的样本** —— 自动匹配到的一律不入库,
    /// 否则会档案污染(profile drift),准确率持续下降且用户看不出来。
    pub fn enroll(
        &self,
        profile_id: Option<&str>,
        display_name: &str,
        embedding_model: &str,
        samples: &[EnrollSample],
    ) -> Result<(String, usize, usize)> {
        let acceptable: Vec<&EnrollSample> =
            samples.iter().filter(|s| s.is_acceptable()).collect();
        let rejected = samples.len() - acceptable.len();

        if acceptable.is_empty() {
            return Err(anyhow!(
                "没有合格样本(需 ≥{} 秒且电平正常),共收到 {} 条",
                MIN_SAMPLE_MS / 1000,
                samples.len()
            ));
        }

        let pid = match profile_id {
            Some(p) => p.to_string(),
            None => self.create_profile(display_name, embedding_model)?,
        };

        // 逐条插入原始样本(保留原始 → 中心点可重算/可回滚)
        for s in &acceptable {
            let sid = format!(
                "{}-{}-{}",
                pid,
                s.session_id,
                s.start_ms
            );
            self.db.conn().execute(
                "INSERT INTO speaker_enrollment_samples
                   (sample_id, profile_id, session_id, embedding, quality_score,
                    duration_ms, weight, added_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(sample_id) DO NOTHING",
                params![
                    sid,
                    pid,
                    s.session_id,
                    serde_json::to_vec(&s.embedding)?,
                    s.quality,
                    s.duration_ms() as i64,
                    // 权重:质量优先;时长给温和增益(平方根),避免超长录音独占
                    s.quality.max(1e-3) * (s.duration_ms() as f32 / 1000.0).sqrt(),
                    now_ms(),
                ],
            )?;
        }

        self.recompute_centroid(&pid)?;
        self.sync_registry(&pid)?;
        Ok((pid, acceptable.len(), rejected))
    }

    fn create_profile(&self, display_name: &str, embedding_model: &str) -> Result<String> {
        let pid = format!("p_{}", &uuid_like());
        self.db.conn().execute(
            "INSERT INTO speaker_profiles
               (profile_id, display_name, centroid, embedding_model, sample_count,
                note, created_at, updated_at)
             VALUES(?1,?2,NULL,?3,0,NULL,?4,?4)",
            params![pid, display_name, embedding_model, now_ms()],
        )?;
        Ok(pid)
    }

    /// 重算中心点。**这是"保留原始样本"的兑现处** ——
    /// 加权方式或 top_k 改了,直接重算即可,旧数据不会作废。
    pub fn recompute_centroid(&self, profile_id: &str) -> Result<()> {
        let mut stmt = self.db.conn().prepare(
            "SELECT embedding, quality_score, duration_ms
             FROM speaker_enrollment_samples WHERE profile_id = ?1",
        )?;
        let rows = stmt.query_map(params![profile_id], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;

        let mut samples: Vec<Embedding> = Vec::new();
        for row in rows {
            let (blob, quality, dur) = row?;
            let mut vector: Vec<f32> = serde_json::from_slice(&blob)?;
            let mut e = Embedding {
                start_ms: 0,
                end_ms: dur as u64,
                vector: std::mem::take(&mut vector),
                quality: quality as f32,
            };
            // ★ 各样本先 L2 归一化,余弦相似度才退化成点积
            e.normalize();
            samples.push(e);
        }

        let centroid = weighted_centroid(&samples, TOP_K_SAMPLES)
            .ok_or_else(|| anyhow!("无法计算中心点:没有有效样本"))?;

        self.db.conn().execute(
            "UPDATE speaker_profiles
             SET centroid = ?2, sample_count = ?3, updated_at = ?4
             WHERE profile_id = ?1",
            params![
                profile_id,
                serde_json::to_vec(&centroid)?,
                samples.len() as i64,
                now_ms()
            ],
        )?;
        Ok(())
    }

    /// 把档案的 ID 与名字同步到 `profiles.json`(**不含向量**)。
    fn sync_registry(&self, profile_id: &str) -> Result<()> {
        let mut reg = self.files.read_registry()?;
        let name: Option<String> = self
            .db
            .conn()
            .query_row(
                "SELECT display_name FROM speaker_profiles WHERE profile_id = ?1",
                params![profile_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(n) = name {
            reg.profiles.insert(
                profile_id.to_string(),
                ProfileEntry {
                    display_name: n,
                    note: None,
                    updated_at: now_ms(),
                },
            );
            self.files.write_registry(&reg)?;
        }
        Ok(())
    }

    /// 改名。
    pub fn rename_profile(&self, profile_id: &str, new_name: &str) -> Result<()> {
        self.db.conn().execute(
            "UPDATE speaker_profiles SET display_name = ?2, updated_at = ?3
             WHERE profile_id = ?1",
            params![profile_id, new_name, now_ms()],
        )?;
        self.sync_registry(profile_id)
    }

    /// 删除档案(样本随外键级联删除)。
    pub fn delete_profile(&self, profile_id: &str) -> Result<()> {
        self.db
            .conn()
            .execute("PRAGMA foreign_keys = ON", [])?;
        self.db.conn().execute(
            "DELETE FROM speaker_profiles WHERE profile_id = ?1",
            params![profile_id],
        )?;
        let mut reg = self.files.read_registry()?;
        reg.profiles.remove(profile_id);
        self.files.write_registry(&reg)?;
        Ok(())
    }

    // -------------------------------------------------------------------
    // 辨认
    // -------------------------------------------------------------------

    /// 1:N 识别:把会话内的说话人簇匹配到已有档案。
    ///
    /// `clusters` 是 (speaker_id, 该簇的嵌入列表)。
    pub fn identify(
        &self,
        clusters: &[(u32, Vec<Embedding>)],
        profiles: &[SpeakerProfile],
    ) -> Vec<MatchResult> {
        clusters
            .iter()
            .map(|(speaker_id, embs)| {
                // 用簇的中心去比对,而不是逐条比对再投票 —— 更稳
                let centroid = weighted_centroid(embs, TOP_K_SAMPLES).unwrap_or_default();

                let mut best: Option<(&SpeakerProfile, f32)> = None;
                for p in profiles {
                    if p.centroid.len() != centroid.len() || centroid.is_empty() {
                        continue;
                    }
                    let sim = cosine_similarity(&centroid, &p.centroid);
                    if best.map(|(_, s)| sim > s).unwrap_or(true) {
                        best = Some((p, sim));
                    }
                }

                match best {
                    Some((p, sim)) => {
                        let conf = MatchConfidence::from_similarity(sim);
                        MatchResult {
                            speaker_id: *speaker_id,
                            profile_id: if conf == MatchConfidence::New {
                                None
                            } else {
                                Some(p.profile_id.clone())
                            },
                            display_name: if conf == MatchConfidence::New {
                                None
                            } else {
                                Some(p.display_name.clone())
                            },
                            similarity: sim,
                            confidence: conf,
                        }
                    }
                    None => MatchResult {
                        speaker_id: *speaker_id,
                        profile_id: None,
                        display_name: None,
                        similarity: 0.0,
                        confidence: MatchConfidence::New,
                    },
                }
            })
            .collect()
    }

    /// 列出全部档案摘要(给 UI 的档案页)。
    pub fn list_profiles(&self) -> Result<Vec<SpeakerProfile>> {
        let mut stmt = self.db.conn().prepare(
            "SELECT profile_id, display_name, centroid, embedding_model, sample_count, note
             FROM speaker_profiles ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<Vec<u8>>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name, blob, model, count, note) = row?;
            let centroid: Vec<f32> = blob
                .as_deref()
                .map(serde_json::from_slice)
                .transpose()?
                .unwrap_or_default();
            out.push(SpeakerProfile {
                profile_id: id,
                display_name: name,
                centroid,
                embedding_model: model,
                sample_count: count as u32,
                note,
            });
        }
        Ok(out)
    }

    /// 某档案的登记样本数(用于"基于 N 次录音")。
    pub fn sample_stats(&self, profile_id: &str) -> Result<(usize, u64)> {
        let mut stmt = self.db.conn().prepare(
            "SELECT COUNT(*), COALESCE(SUM(duration_ms),0)
             FROM speaker_enrollment_samples WHERE profile_id = ?1",
        )?;
        let (n, ms): (i64, i64) = stmt.query_row(params![profile_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok((n as usize, ms as u64))
    }
}

/// 不需要 uuid crate 的轻量 ID。
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("{nanos:x}{pid:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::files::FileStore;

    struct H {
        _dir: tempfile::TempDir,
        db: Db,
        files: FileStore,
    }

    fn harness() -> H {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("c.db")).unwrap();
        let files = FileStore::new(dir.path().join("store"));
        files.ensure_dirs().unwrap();
        H {
            _dir: dir,
            db,
            files,
        }
    }

    fn emb(seed: f32, len: usize) -> Vec<f32> {
        (0..len).map(|i| if i == 0 { seed } else { 0.01 * i as f32 }).collect()
    }

    fn sample(session: &str, seed: f32, dur_ms: u64) -> EnrollSample {
        EnrollSample {
            session_id: session.into(),
            embedding: emb(seed, 8),
            start_ms: 0,
            end_ms: dur_ms,
            quality: 1.0,
        }
    }

    const MODEL: &str = "3dspeaker";

    #[test]
    fn threshold_mapping_is_three_tier() {
        assert_eq!(
            MatchConfidence::from_similarity(0.95),
            MatchConfidence::Confirmed
        );
        assert_eq!(
            MatchConfidence::from_similarity(0.72),
            MatchConfidence::Confirmed
        );
        assert_eq!(
            MatchConfidence::from_similarity(0.60),
            MatchConfidence::Uncertain,
            "中间档是核心设计:它让不可见的错误变得可见"
        );
        assert_eq!(MatchConfidence::from_similarity(0.55), MatchConfidence::Uncertain);
        assert_eq!(MatchConfidence::from_similarity(0.30), MatchConfidence::New);
    }

    #[test]
    fn enroll_creates_profile_and_centroid() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        // 两条样本必须起点不同 —— 同一会话同一起点会被视为同一条(去重)
        let mut s1 = sample("s1", 1.0, 10_000);
        let mut s2 = sample("s1", 1.0, 12_000);
        s1.start_ms = 0;
        s2.start_ms = 20_000;
        s2.end_ms = 32_000;

        let (pid, accepted, rejected) = vs.enroll(None, "张老师", MODEL, &[s1, s2]).unwrap();
        assert_eq!(accepted, 2);
        assert_eq!(rejected, 0);

        let profiles = vs.load_profiles(MODEL).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].display_name, "张老师");
        assert!(!profiles[0].centroid.is_empty());
        assert_eq!(profiles[0].sample_count, 2);
        assert_eq!(profiles[0].profile_id, pid);
    }

    #[test]
    fn short_samples_are_rejected() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        let err = vs.enroll(None, "X", MODEL, &[sample("s1", 1.0, 500)]);
        assert!(err.is_err(), "★ 过短样本必须被拒绝,否则污染档案");
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("没有合格样本"), "{msg}");
    }

    #[test]
    fn mixed_samples_only_accept_good_ones() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        let (_, accepted, rejected) = vs
            .enroll(
                None,
                "张老师",
                MODEL,
                &[
                    sample("s1", 1.0, 10_000),
                    sample("s1", 1.0, 200), // 太短
                    sample("s1", 1.0, 8000),
                ],
            )
            .unwrap();
        assert_eq!(accepted, 2);
        assert_eq!(rejected, 1);
    }

    #[test]
    fn profiles_are_persisted_and_listed() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        vs.enroll(None, "甲", MODEL, &[sample("s1", 1.0, 10_000)])
            .unwrap();
        vs.enroll(None, "乙", MODEL, &[sample("s2", 0.0, 10_000)])
            .unwrap();
        let list = vs.list_profiles().unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|p| p.display_name == "甲"));
        assert!(list.iter().any(|p| p.display_name == "乙"));
    }

    #[test]
    fn registry_contains_names_but_not_vectors() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        vs.enroll(None, "张老师", MODEL, &[sample("s1", 1.0, 10_000)])
            .unwrap();

        // ★ 声纹向量永不出本机:profiles.json 里只能有 ID 和名字
        let raw = std::fs::read_to_string(h.files.registry_path()).unwrap();
        assert!(raw.contains("张老师"), "注册表应有名字");
        assert!(
            !raw.contains("centroid") && !raw.contains("embedding"),
            "★ profiles.json 绝不能包含向量: {raw}"
        );
        let reg = h.files.read_registry().unwrap();
        assert_eq!(reg.profiles.len(), 1);
    }

    #[test]
    fn model_mismatch_disables_profiles() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        vs.enroll(None, "张老师", "old-model", &[sample("s1", 1.0, 10_000)])
            .unwrap();

        // ★ 换声纹模型 = 向量空间变了 → 旧档案必须失效,而不是给出错误匹配
        let with_new = vs.load_profiles("new-model").unwrap();
        assert!(with_new.is_empty(), "模型不匹配时档案必须被跳过");
        let with_old = vs.load_profiles("old-model").unwrap();
        assert_eq!(with_old.len(), 1);
    }

    #[test]
    fn identify_matches_identical_voice() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        let (pid, _, _) = vs
            .enroll(None, "张老师", MODEL, &[sample("s1", 1.0, 10_000)])
            .unwrap();

        let profiles = vs.load_profiles(MODEL).unwrap();
        let clusters = vec![(
            0u32,
            vec![Embedding {
                start_ms: 0,
                end_ms: 5000,
                vector: emb(1.0, 8),
                quality: 1.0,
            }],
        )];
        let res = vs.identify(&clusters, &profiles);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].confidence, MatchConfidence::Confirmed);
        assert_eq!(res[0].profile_id.as_deref(), Some(pid.as_str()));
        assert_eq!(res[0].display_name.as_deref(), Some("张老师"));
    }

    #[test]
    fn identify_marks_unknown_voice_as_new() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        vs.enroll(None, "张老师", MODEL, &[sample("s1", 1.0, 10_000)])
            .unwrap();
        let profiles = vs.load_profiles(MODEL).unwrap();

        // 正交方向的向量 → 不同人
        let clusters = vec![(
            1u32,
            vec![Embedding {
                start_ms: 0,
                end_ms: 5000,
                vector: vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                quality: 1.0,
            }],
        )];
        let res = vs.identify(&clusters, &profiles);
        assert_eq!(res[0].confidence, MatchConfidence::New);
        assert!(res[0].profile_id.is_none(), "未匹配时不应硬塞一个身份");
        assert!(res[0].display_name.is_none());
    }

    #[test]
    fn identify_with_no_profiles_returns_new() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        let clusters = vec![(
            0u32,
            vec![Embedding {
                start_ms: 0,
                end_ms: 5000,
                vector: emb(1.0, 8),
                quality: 1.0,
            }],
        )];
        let res = vs.identify(&clusters, &[]);
        assert_eq!(res[0].confidence, MatchConfidence::New);
    }

    #[test]
    fn centroid_recomputable_from_raw_samples() {
        // ★ 保留原始样本的意义:改算法后能重算,旧数据不作废
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        let (pid, _, _) = vs
            .enroll(None, "张老师", MODEL, &[sample("s1", 1.0, 10_000)])
            .unwrap();
        let before = vs.load_profiles(MODEL).unwrap()[0].centroid.clone();

        // 再加一条样本 → 中心点应变化
        vs.enroll(
            Some(&pid),
            "张老师",
            MODEL,
            &[sample("s2", 1.0, 15_000)],
        )
        .unwrap();
        let after = vs.load_profiles(MODEL).unwrap()[0].centroid.clone();
        assert_eq!(before.len(), after.len());
        assert!(!after.is_empty());
        assert_eq!(vs.sample_stats(&pid).unwrap().0, 2);
    }

    #[test]
    fn enroll_into_existing_profile_accumulates() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        let (pid, _, _) = vs
            .enroll(None, "张老师", MODEL, &[sample("s1", 1.0, 10_000)])
            .unwrap();
        vs.enroll(Some(&pid), "张老师", MODEL, &[sample("s2", 1.0, 12_000)])
            .unwrap();
        let profiles = vs.load_profiles(MODEL).unwrap();
        assert_eq!(profiles.len(), 1, "不应新建第二个档案");
        assert_eq!(profiles[0].sample_count, 2);
    }

    #[test]
    fn duplicate_sample_is_not_double_counted() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        let (pid, _, _) = vs
            .enroll(None, "张老师", MODEL, &[sample("s1", 1.0, 10_000)])
            .unwrap();
        // 同一会话同一起点 → 视为同一条样本
        vs.enroll(Some(&pid), "张老师", MODEL, &[sample("s1", 1.0, 10_000)])
            .unwrap();
        assert_eq!(vs.sample_stats(&pid).unwrap().0, 1, "重复样本不应重复计入");
    }

    #[test]
    fn rename_profile_updates_registry() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        let (pid, _, _) = vs
            .enroll(None, "说话人 1", MODEL, &[sample("s1", 1.0, 10_000)])
            .unwrap();
        vs.rename_profile(&pid, "张老师").unwrap();

        assert_eq!(vs.load_profiles(MODEL).unwrap()[0].display_name, "张老师");
        let reg = h.files.read_registry().unwrap();
        assert_eq!(reg.profiles[&pid].display_name, "张老师");
    }

    #[test]
    fn delete_profile_removes_samples_and_registry_entry() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        let (pid, _, _) = vs
            .enroll(None, "张老师", MODEL, &[sample("s1", 1.0, 10_000)])
            .unwrap();
        assert_eq!(vs.sample_stats(&pid).unwrap().0, 1);

        vs.delete_profile(&pid).unwrap();
        assert!(vs.list_profiles().unwrap().is_empty());
        assert_eq!(vs.sample_stats(&pid).unwrap().0, 0, "样本应级联删除");
        assert!(h.files.read_registry().unwrap().profiles.is_empty());
    }

    #[test]
    fn quality_gate_blocks_zero_quality() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        let mut s = sample("s1", 1.0, 10_000);
        s.quality = 0.0;
        assert!(vs.enroll(None, "X", MODEL, &[s]).is_err());
    }

    #[test]
    fn empty_embedding_is_rejected() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        let mut s = sample("s1", 1.0, 10_000);
        s.embedding = vec![];
        assert!(vs.enroll(None, "X", MODEL, &[s]).is_err());
    }

    #[test]
    fn model_mismatch_skips_and_reports() {
        let h = harness();
        let vs = VoiceprintStore::new(&h.db, &h.files);
        vs.enroll(None, "A", "m1", &[sample("s1", 1.0, 10_000)])
            .unwrap();
        vs.enroll(None, "B", "m2", &[sample("s2", 1.0, 10_000)])
            .unwrap();
        // 只应加载匹配的
        assert_eq!(vs.load_profiles("m1").unwrap().len(), 1);
        assert_eq!(vs.load_profiles("m2").unwrap().len(), 1);
        assert_eq!(vs.load_profiles("m3").unwrap().len(), 0);
    }
}
