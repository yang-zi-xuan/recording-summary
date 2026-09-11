//! 缓存键与读写。
//!
//! 见技术方案 §4.8。三个关键点:
//!
//! - **`backend` 必须进键** —— 同一模型跨后端浮点累加顺序不同,结果会有细微差异。
//!   不进键就会遇到"在 A 机器转写、拷到 B 机器后结果对不上"。
//! - **`language` 也要进键** —— 中文会自动附加简体提示,prompt 变了结果就会变。
//! - **`chunk_index` 支持分块缓存** —— CPU 模式下断点续传的基础。
//! - **名字不进键** —— 改名是展示层的事,不触发重新转写;这决定了迭代速度。

use crate::store::db::{now_ms, Db};
use crate::types::{Backend, DiarizeMode, Transcript};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// 缓存键
// ---------------------------------------------------------------------------

/// 转写缓存的全部输入。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CacheKeyInputs {
    pub audio_hash: String,
    pub asr_model: String,
    pub backend: Backend,
    /// ★ 语言必须进键 —— 中文会自动附加简体提示(prompt 变了 → 结果会变)
    pub language: Option<String>,
    pub diarize_enabled: bool,
    pub diarize_model: Option<String>,
    pub num_speakers: DiarizeMode,
    pub vad: bool,
    pub hotwords_version: u32,
    /// 分块转写的块号。None 表示整段一次性转写。
    pub chunk_index: Option<u32>,
}

/// 转写缓存键。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TranscriptCacheKey(String);

impl TranscriptCacheKey {
    pub fn compute(inputs: &CacheKeyInputs) -> Self {
        let mut h = Sha256::new();
        h.update(inputs.audio_hash.as_bytes());
        h.update(b"|");
        h.update(inputs.asr_model.as_bytes());
        h.update(b"|");
        h.update(inputs.backend.as_str().as_bytes());
        h.update(b"|");
        h.update(inputs.language.as_deref().unwrap_or("auto").as_bytes());
        h.update(b"|");
        h.update([inputs.diarize_enabled as u8]);
        h.update(b"|");
        h.update(inputs.diarize_model.as_deref().unwrap_or("-").as_bytes());
        h.update(b"|");
        h.update(inputs.num_speakers.cache_tag().as_bytes());
        h.update(b"|");
        h.update([inputs.vad as u8]);
        h.update(b"|");
        h.update(inputs.hotwords_version.to_le_bytes());
        h.update(b"|");
        h.update(inputs.chunk_index.unwrap_or(u32::MAX).to_le_bytes());
        Self(hex::encode(h.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TranscriptCacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 说话人区分结果缓存键。
///
/// **刻意不含 `DiarizeMode`** —— 用户会反复改人数试效果。如果把模式也进键,
/// 每次试都是一个新条目,缓存就形同虚设。按「音频 + 模型」存,
/// 改人数时复用同一份分割结果。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DiarizeCacheKey(String);

impl DiarizeCacheKey {
    pub fn compute(audio_hash: &str, seg_model: &str, emb_model: &str, backend: Backend) -> Self {
        let mut h = Sha256::new();
        h.update(audio_hash.as_bytes());
        h.update(b"|diarize|");
        h.update(seg_model.as_bytes());
        h.update(b"|");
        h.update(emb_model.as_bytes());
        h.update(b"|");
        h.update(backend.as_str().as_bytes());
        Self(hex::encode(h.finalize()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// 说话人区分缓存
// ---------------------------------------------------------------------------

pub fn put_diarize(
    db: &Db,
    key: &DiarizeCacheKey,
    session_id: &str,
    intervals_json: &str,
) -> Result<()> {
    db.conn().execute(
        "INSERT INTO diarize_cache(cache_key, session_id, intervals_json, created_at)
         VALUES(?1,?2,?3,?4)
         ON CONFLICT(cache_key) DO UPDATE SET
            intervals_json = excluded.intervals_json,
            created_at     = excluded.created_at",
        params![key.as_str(), session_id, intervals_json, now_ms()],
    )?;
    Ok(())
}

pub fn get_diarize(db: &Db, key: &DiarizeCacheKey) -> Result<Option<String>> {
    let v = db
        .conn()
        .query_row(
            "SELECT intervals_json FROM diarize_cache WHERE cache_key = ?1",
            params![key.as_str()],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    Ok(v)
}

// ---------------------------------------------------------------------------
// 缓存读写
// ---------------------------------------------------------------------------

/// 转写缓存的持久化(分段存储,支持分块)。
pub fn put_transcript(
    db: &Db,
    key: &TranscriptCacheKey,
    session_id: &str,
    chunk_index: Option<u32>,
    segments_json: &str,
) -> Result<()> {
    db.conn().execute(
        "INSERT INTO transcript_cache(cache_key, session_id, chunk_index, segments_json, created_at)
         VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(cache_key) DO UPDATE SET
            segments_json = excluded.segments_json,
            created_at    = excluded.created_at",
        params![
            key.as_str(),
            session_id,
            chunk_index.map(|c| c as i64),
            segments_json,
            now_ms()
        ],
    )?;
    Ok(())
}

pub fn get_transcript(db: &Db, key: &TranscriptCacheKey) -> Result<Option<String>> {
    let v = db
        .conn()
        .query_row(
            "SELECT segments_json FROM transcript_cache WHERE cache_key = ?1",
            params![key.as_str()],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    Ok(v)
}

/// 取某会话全部已缓存的块,按块号排序。用于断点续传与结果拼接。
pub fn get_chunks(db: &Db, session_id: &str) -> Result<Vec<(Option<u32>, String)>> {
    let mut stmt = db.conn().prepare(
        "SELECT chunk_index, segments_json FROM transcript_cache
         WHERE session_id = ?1
         ORDER BY COALESCE(chunk_index, -1)",
    )?;
    let rows = stmt
        .query_map(params![session_id], |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?.map(|v| v as u32),
                r.get::<_, String>(1)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 把已完成的块拼成一份完整转写。中断后重跑时就靠这个保住已完成的部分。
pub fn assemble_transcript(
    db: &Db,
    session_id: &str,
    engine: &str,
    model: &str,
    backend: Backend,
    duration_ms: u64,
) -> Result<Option<Transcript>> {
    let chunks = get_chunks(db, session_id)?;
    if chunks.is_empty() {
        return Ok(None);
    }
    let mut segments = Vec::new();
    for (_, json) in &chunks {
        let mut part: Vec<crate::types::Segment> = serde_json::from_str(json)?;
        segments.append(&mut part);
    }
    segments.sort_by_key(|s| s.start_ms);
    let raw_text: String = segments.iter().map(|s| s.text.as_str()).collect();
    Ok(Some(Transcript {
        engine: engine.to_string(),
        model: model.to_string(),
        backend,
        backend_diarize: None,
        language: None,
        segments,
        duration_ms,
        raw_text,
        diarize: None,
    }))
}

/// 已完成的块号集合 —— 用来跳过重跑。
pub fn completed_chunks(db: &Db, session_id: &str) -> Result<Vec<u32>> {
    Ok(get_chunks(db, session_id)?
        .into_iter()
        .filter_map(|(i, _)| i)
        .collect())
}

// ---------------------------------------------------------------------------
// 缓存统计
// ---------------------------------------------------------------------------

/// 缓存统计,给 CLI 的 `stats` 子命令用。
#[derive(Clone, Debug, Default)]
pub struct CacheStats {
    pub sessions: i64,
    pub transcript_chunks: i64,
    pub diarize_results: i64,
    pub enrollment_samples: i64,
}

pub fn stats(db: &Db) -> Result<CacheStats> {
    let one = |sql: &str| -> Result<i64> { Ok(db.conn().query_row(sql, [], |r| r.get(0))?) };
    Ok(CacheStats {
        sessions: one("SELECT COUNT(*) FROM sessions")?,
        transcript_chunks: one("SELECT COUNT(*) FROM transcript_cache")?,
        diarize_results: one("SELECT COUNT(*) FROM diarize_cache")?,
        enrollment_samples: one("SELECT COUNT(*) FROM speaker_enrollment_samples")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diarize::Embedding;

    fn base_inputs() -> CacheKeyInputs {
        CacheKeyInputs {
            audio_hash: "deadbeef".into(),
            asr_model: "large-v3-turbo".into(),
            backend: Backend::Cuda,
            language: Some("zh".into()),
            diarize_enabled: true,
            diarize_model: Some("3dspeaker".into()),
            num_speakers: DiarizeMode::Fixed(4),
            vad: true,
            hotwords_version: 1,
            chunk_index: None,
        }
    }

    #[test]
    fn same_inputs_same_key() {
        let a = TranscriptCacheKey::compute(&base_inputs());
        let b = TranscriptCacheKey::compute(&base_inputs());
        assert_eq!(a, b);
    }

    #[test]
    fn backend_changes_key() {
        let mut i1 = base_inputs();
        let mut i2 = base_inputs();
        i1.backend = Backend::Cpu;
        i2.backend = Backend::Cuda;
        assert_ne!(
            TranscriptCacheKey::compute(&i1),
            TranscriptCacheKey::compute(&i2),
            "★ 后端必须影响缓存键,否则跨机器结果会对不上"
        );
    }

    #[test]
    fn language_changes_key() {
        // ★ 语言进键:中文会自动附加简体提示,prompt 变了结果就会变
        let mut i1 = base_inputs();
        let mut i2 = base_inputs();
        i1.language = Some("zh".into());
        i2.language = Some("en".into());
        assert_ne!(
            TranscriptCacheKey::compute(&i1),
            TranscriptCacheKey::compute(&i2),
            "换语言必须重新转写"
        );

        let mut i3 = base_inputs();
        i3.language = None;
        assert_ne!(
            TranscriptCacheKey::compute(&i1),
            TranscriptCacheKey::compute(&i3),
            "auto 与显式语言也应区分"
        );
    }

    #[test]
    fn chunk_index_changes_key() {
        let mut i1 = base_inputs();
        let mut i2 = base_inputs();
        i1.chunk_index = Some(0);
        i2.chunk_index = Some(1);
        assert_ne!(TranscriptCacheKey::compute(&i1), TranscriptCacheKey::compute(&i2));
    }

    #[test]
    fn speaker_count_changes_key() {
        let mut i1 = base_inputs();
        let mut i2 = base_inputs();
        i1.num_speakers = DiarizeMode::Fixed(3);
        i2.num_speakers = DiarizeMode::Fixed(5);
        assert_ne!(TranscriptCacheKey::compute(&i1), TranscriptCacheKey::compute(&i2));
    }

    #[test]
    fn hotwords_version_changes_key() {
        let mut i1 = base_inputs();
        let mut i2 = base_inputs();
        i2.hotwords_version = 2;
        assert_ne!(TranscriptCacheKey::compute(&i1), TranscriptCacheKey::compute(&i2));
    }

    #[test]
    fn transcript_chunk_roundtrip_and_assembly() {
        let db = Db::open_in_memory().unwrap();
        let mut inp = base_inputs();
        let segs0 = serde_json::to_string(&vec![
            crate::types::Segment::new(0, 1000, "第一段"),
        ])
        .unwrap();
        let segs1 = serde_json::to_string(&vec![
            crate::types::Segment::new(1000, 2000, "第二段"),
        ])
        .unwrap();

        inp.chunk_index = Some(0);
        let k0 = TranscriptCacheKey::compute(&inp);
        put_transcript(&db, &k0, "s1", Some(0), &segs0).unwrap();

        inp.chunk_index = Some(1);
        let k1 = TranscriptCacheKey::compute(&inp);
        put_transcript(&db, &k1, "s1", Some(1), &segs1).unwrap();

        assert!(get_transcript(&db, &k0).unwrap().is_some());
        assert_eq!(completed_chunks(&db, "s1").unwrap(), vec![0, 1]);

        let full = assemble_transcript(&db, "s1", "whisper.cpp", "m", Backend::Cpu, 2000)
            .unwrap()
            .unwrap();
        assert_eq!(full.segments.len(), 2);
        assert_eq!(full.segments[0].text, "第一段");
        assert_eq!(full.raw_text, "第一段第二段");
        assert_eq!(full.duration_ms, 2000);
        // 拼接后必须按时间排序
        assert!(full.segments[0].start_ms <= full.segments[1].start_ms);
    }

    #[test]
    fn assembly_returns_none_when_empty() {
        let db = Db::open_in_memory().unwrap();
        assert!(assemble_transcript(&db, "nope", "e", "m", Backend::Cpu, 0)
            .unwrap()
            .is_none());
    }

    #[test]
    fn diarize_cache_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let key = DiarizeCacheKey::compute("h", "seg", "emb", Backend::Cpu);
        assert!(get_diarize(&db, &key).unwrap().is_none());

        let intervals = vec![
            crate::diarize::SpeakerInterval {
                start_ms: 0,
                end_ms: 1200,
                speaker_id: 0,
            },
            crate::diarize::SpeakerInterval {
                start_ms: 1200,
                end_ms: 2000,
                speaker_id: 1,
            },
        ];
        let json = serde_json::to_string(&intervals).unwrap();
        put_diarize(&db, &key, "s1", &json).unwrap();

        let back = get_diarize(&db, &key).unwrap().unwrap();
        let parsed: Vec<crate::diarize::SpeakerInterval> = serde_json::from_str(&back).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].speaker_id, 1);
        assert_eq!(parsed[1].end_ms, 2000);
    }

    #[test]
    fn diarize_key_ignores_speaker_count() {
        // ★ 这是刻意的:用户会反复改人数试效果。
        //   如果把 DiarizeMode 也进键,每次试都是一个新条目,缓存就没用了。
        let a = DiarizeCacheKey::compute("h", "seg", "emb", Backend::Cpu);
        let b = DiarizeCacheKey::compute("h", "seg", "emb", Backend::Cpu);
        assert_eq!(a.as_str(), b.as_str(), "同一音频+模型必须命中同一份缓存");
    }

    #[test]
    fn diarize_key_differs_by_model_and_backend() {
        let base = DiarizeCacheKey::compute("h", "seg", "emb", Backend::Cpu);
        assert_ne!(
            base.as_str(),
            DiarizeCacheKey::compute("h", "seg2", "emb", Backend::Cpu).as_str()
        );
        assert_ne!(
            base.as_str(),
            DiarizeCacheKey::compute("h", "seg", "emb2", Backend::Cpu).as_str()
        );
        assert_ne!(
            base.as_str(),
            DiarizeCacheKey::compute("h", "seg", "emb", Backend::Cuda).as_str()
        );
        assert_ne!(
            base.as_str(),
            DiarizeCacheKey::compute("h2", "seg", "emb", Backend::Cpu).as_str()
        );
    }

    #[test]
    fn diarize_and_transcript_keys_do_not_collide() {
        // 两者共用同一张表前缀语义,键必须可区分
        let d = DiarizeCacheKey::compute("h", "a", "b", Backend::Cpu);
        let t = TranscriptCacheKey::compute(&CacheKeyInputs {
            audio_hash: "h".into(),
            asr_model: "a".into(),
            backend: Backend::Cpu,
            language: None,
            diarize_enabled: false,
            diarize_model: None,
            num_speakers: DiarizeMode::Auto,
            vad: false,
            hotwords_version: 0,
            chunk_index: None,
        });
        assert_ne!(d.as_str(), t.as_str());
    }

    #[test]
    fn stats_counts_rows() {
        let db = Db::open_in_memory().unwrap();
        let s = stats(&db).unwrap();
        assert_eq!(s.sessions, 0);
        assert_eq!(s.transcript_chunks, 0);
    }
}
