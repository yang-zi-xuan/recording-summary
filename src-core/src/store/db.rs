//! SQLite 本地缓存层。
//!
//! 见技术方案 §5.2。数据库是**可重建的缓存**,真相源是 `store/files` 里的文件。
//! `hardware_profile` 明确不进同步 —— 每台机器硬件不同。

use crate::hardware::HardwareProfile;
use crate::types::{Backend, DiarizeMode, Scene, SpeakerLabels, Transcript};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct Db {
    conn: Connection,
}

/// 会话索引行。
#[derive(Clone, Debug)]
pub struct SessionRow {
    pub id: String,
    pub title: Option<String>,
    pub created_at: i64,
    pub duration_ms: u64,
    pub audio_local_path: Option<String>,
    pub source_type: String,
    pub status: String,
    pub scene: Option<Scene>,
    pub scene_confidence: Option<f32>,
    pub last_modified_at: i64,
    pub origin_device: Option<String>,
}

impl SessionRow {
    /// 造一条最小可用的会话记录,给测试用。
    ///
    /// 字段多但大多有合理默认值,测试里只关心其中一两个 ——
    /// 让调用方按需覆盖,比每次写全 11 个字段省事得多。
    #[doc(hidden)]
    pub fn new_for_test(id: &str) -> Self {
        Self {
            id: id.to_string(),
            title: None,
            created_at: now_ms(),
            duration_ms: 0,
            audio_local_path: None,
            source_type: "file".into(),
            status: "done".into(),
            scene: None,
            scene_confidence: None,
            last_modified_at: now_ms(),
            origin_device: None,
        }
    }
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建数据目录失败: {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开数据库失败: {}", path.display()))?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self> {
        let db = Self {
            conn: Connection::open_in_memory()?,
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS sessions (
                id                TEXT PRIMARY KEY,
                title             TEXT,
                created_at        INTEGER NOT NULL,
                duration_ms       INTEGER NOT NULL,
                audio_local_path  TEXT,
                source_type       TEXT NOT NULL,
                has_loopback_track INTEGER DEFAULT 0,
                has_mic_track     INTEGER DEFAULT 0,
                status            TEXT NOT NULL,
                scene             TEXT,
                scene_confidence  REAL,
                scene_evidence    TEXT,
                last_modified_at  INTEGER NOT NULL,
                origin_device     TEXT
            );

            CREATE TABLE IF NOT EXISTS transcript_cache (
                cache_key     TEXT PRIMARY KEY,
                session_id    TEXT NOT NULL,
                chunk_index   INTEGER,
                segments_json TEXT NOT NULL,
                created_at    INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_tc_session
                ON transcript_cache(session_id, chunk_index);

            CREATE TABLE IF NOT EXISTS speaker_embeddings (
                cache_key          TEXT PRIMARY KEY,
                session_id         TEXT NOT NULL,
                embeddings_blob    BLOB NOT NULL,
                segment_index_json TEXT NOT NULL,
                created_at         INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS hardware_profile (
                id                 INTEGER PRIMARY KEY CHECK (id = 1),
                backend_selected   TEXT NOT NULL,
                backend_requested  TEXT,
                device_name        TEXT,
                vram_gb            REAL,
                cpu_cores          INTEGER,
                rtf_measured       REAL,
                model_recommended  TEXT,
                probed_at          INTEGER NOT NULL,
                probe_log          TEXT
            );

            -- 说话人区分结果缓存(分割 + 聚类后的时间区间)。
            -- 与 speaker_embeddings 分开:后者服务于声纹登记,语义不同。
            CREATE TABLE IF NOT EXISTS diarize_cache (
                cache_key      TEXT PRIMARY KEY,
                session_id     TEXT NOT NULL,
                intervals_json TEXT NOT NULL,
                created_at     INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS speaker_profiles (
                profile_id      TEXT PRIMARY KEY,
                display_name    TEXT NOT NULL,
                centroid        BLOB,
                embedding_model TEXT NOT NULL,
                sample_count    INTEGER DEFAULT 0,
                note            TEXT,
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS speaker_enrollment_samples (
                sample_id      TEXT PRIMARY KEY,
                profile_id     TEXT NOT NULL
                                REFERENCES speaker_profiles(profile_id) ON DELETE CASCADE,
                session_id     TEXT NOT NULL,
                embedding      BLOB NOT NULL,
                quality_score  REAL NOT NULL,
                duration_ms    INTEGER NOT NULL,
                weight         REAL NOT NULL,
                added_at       INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_samples_profile
                ON speaker_enrollment_samples(profile_id);

            CREATE TABLE IF NOT EXISTS session_speaker_matches (
                session_id         TEXT NOT NULL,
                speaker_id         INTEGER NOT NULL,
                matched_profile_id TEXT,
                similarity         REAL,
                confidence         TEXT NOT NULL,
                PRIMARY KEY (session_id, speaker_id)
            );

            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 会话
    // -----------------------------------------------------------------------

    pub fn upsert_session(&self, row: &SessionRow) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO sessions (id, title, created_at, duration_ms, audio_local_path,
                                  source_type, status, scene, scene_confidence,
                                  last_modified_at, origin_device)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
            ON CONFLICT(id) DO UPDATE SET
                title            = excluded.title,
                duration_ms      = excluded.duration_ms,
                audio_local_path = excluded.audio_local_path,
                status           = excluded.status,
                scene            = excluded.scene,
                scene_confidence = excluded.scene_confidence,
                last_modified_at = excluded.last_modified_at
            "#,
            params![
                row.id,
                row.title,
                row.created_at,
                row.duration_ms as i64,
                row.audio_local_path,
                row.source_type,
                row.status,
                row.scene.map(|s| format!("{s:?}").to_lowercase()),
                row.scene_confidence,
                row.last_modified_at,
                row.origin_device,
            ],
        )?;
        Ok(())
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, title, created_at, duration_ms, audio_local_path, source_type,
                        status, scene, scene_confidence, last_modified_at, origin_device
                 FROM sessions WHERE id = ?1",
                params![id],
                |r| {
                    Ok(SessionRow {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        created_at: r.get(2)?,
                        duration_ms: r.get::<_, i64>(3)? as u64,
                        audio_local_path: r.get(4)?,
                        source_type: r.get(5)?,
                        status: r.get(6)?,
                        scene: r
                            .get::<_, Option<String>>(7)?
                            .and_then(|s| Scene::parse(&s)),
                        scene_confidence: r.get(8)?,
                        last_modified_at: r.get(9)?,
                        origin_device: r.get(10)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn set_session_status(&self, id: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET status = ?2, last_modified_at = ?3 WHERE id = ?1",
            params![id, status, now_ms()],
        )?;
        Ok(())
    }

    /// 只改标题。
    ///
    /// # 什么时候需要它
    ///
    /// **工程重命名。** 标题在两处各有一份:
    ///
    /// - `projects/<目录>/project.json` 的 `title` —— 「我的工程」读它
    /// - `sessions.title` —— 「历史记录」和「处理录音」页读它
    ///
    /// 两边用同一个 ID(音频内容哈希)关联,但**是两套独立存储**。
    /// 只改一处会出现"同一个录音在两个页面显示不同名字"。
    pub fn set_session_title(&self, id: &str, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET title = ?2, last_modified_at = ?3 WHERE id = ?1",
            params![id, title, now_ms()],
        )?;
        Ok(())
    }

    /// 会话是否存在。用于"工程改名时顺带补一条会话记录"。
    pub fn session_exists(&self, id: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, created_at, duration_ms, audio_local_path, source_type,
                    status, scene, scene_confidence, last_modified_at, origin_device
             FROM sessions ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok(SessionRow {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    created_at: r.get(2)?,
                    duration_ms: r.get::<_, i64>(3)? as u64,
                    audio_local_path: r.get(4)?,
                    source_type: r.get(5)?,
                    status: r.get(6)?,
                    scene: r.get::<_, Option<String>>(7)?.and_then(|s| Scene::parse(&s)),
                    scene_confidence: r.get(8)?,
                    last_modified_at: r.get(9)?,
                    origin_device: r.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // -----------------------------------------------------------------------
    // 硬件画像
    // -----------------------------------------------------------------------

    pub fn save_hardware_profile(&self, p: &HardwareProfile) -> Result<()> {
        let requested = p.backend_requested.map(|b| b.as_str().to_string());
        let log = serde_json::to_string(&p.probes)?;
        self.conn.execute(
            r#"
            INSERT INTO hardware_profile (id, backend_selected, backend_requested, device_name,
                vram_gb, cpu_cores, rtf_measured, model_recommended, probed_at, probe_log)
            VALUES (1,?1,?2,?3,?4,?5,?6,?7,?8,?9)
            ON CONFLICT(id) DO UPDATE SET
                backend_selected  = excluded.backend_selected,
                backend_requested = excluded.backend_requested,
                device_name       = excluded.device_name,
                vram_gb           = excluded.vram_gb,
                cpu_cores         = excluded.cpu_cores,
                rtf_measured      = excluded.rtf_measured,
                model_recommended = excluded.model_recommended,
                probed_at         = excluded.probed_at,
                probe_log         = excluded.probe_log
            "#,
            params![
                p.backend_selected.as_str(),
                requested,
                p.device_name,
                p.vram_gb,
                p.cpu_cores as i64,
                p.rtf_measured,
                p.model_recommended.label(),
                p.probed_at,
                log,
            ],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 设置
    // -----------------------------------------------------------------------

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings(key,value) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let v = self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v)
    }

    // -----------------------------------------------------------------------
    // 原始访问(供 cache 模块使用)
    // -----------------------------------------------------------------------

    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// 当前时间(毫秒时间戳)。
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 保存说话人标签版本(用于判断总结是否过时)。
pub fn save_labels_version(db: &Db, session_id: &str, labels: &SpeakerLabels) -> Result<()> {
    db.set_setting(
        &format!("labels_version:{session_id}"),
        &labels.labels_version.to_string(),
    )
}

#[allow(dead_code)]
fn _backend_used(_: Backend, _: DiarizeMode, _: &Transcript) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ModelTier;

    #[test]
    fn migrate_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        db.migrate().unwrap();
        db.migrate().unwrap();
    }

    #[test]
    fn session_upsert_and_read() {
        let db = Db::open_in_memory().unwrap();
        let row = SessionRow {
            id: "abc".into(),
            title: Some("第12讲".into()),
            created_at: 1000,
            duration_ms: 2_700_000,
            audio_local_path: Some("D:/a.mp3".into()),
            source_type: "file".into(),
            status: "pending".into(),
            scene: None,
            scene_confidence: None,
            last_modified_at: 1000,
            origin_device: Some("desktop".into()),
        };
        db.upsert_session(&row).unwrap();
        let got = db.get_session("abc").unwrap().unwrap();
        assert_eq!(got.title.as_deref(), Some("第12讲"));
        assert_eq!(got.duration_ms, 2_700_000);

        // 二次写入应更新而不是报错
        let mut row2 = row.clone();
        row2.status = "done".into();
        row2.scene = Some(Scene::Lecture);
        row2.scene_confidence = Some(0.87);
        db.upsert_session(&row2).unwrap();
        let got2 = db.get_session("abc").unwrap().unwrap();
        assert_eq!(got2.status, "done");
        assert_eq!(got2.scene, Some(Scene::Lecture));
        assert_eq!(db.list_sessions(10).unwrap().len(), 1);
    }

    #[test]
    fn missing_session_is_none() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.get_session("nope").unwrap().is_none());
    }

    #[test]
    fn settings_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.get_setting("k").unwrap().is_none());
        db.set_setting("k", "v1").unwrap();
        assert_eq!(db.get_setting("k").unwrap().as_deref(), Some("v1"));
        db.set_setting("k", "v2").unwrap();
        assert_eq!(db.get_setting("k").unwrap().as_deref(), Some("v2"));
    }

    #[test]
    fn hardware_profile_is_single_row() {
        let db = Db::open_in_memory().unwrap();
        let mk = |backend: Backend| HardwareProfile {
            backend_selected: backend,
            backend_requested: None,
            device_name: Some("RTX 4060".into()),
            vram_gb: Some(8.0),
            cpu_cores: 32,
            rtf_measured: 0.05,
            model_recommended: ModelTier::LargeV3Turbo,
            probed_at: 123,
            probes: vec![],
        };
        db.save_hardware_profile(&mk(Backend::Cuda)).unwrap();
        db.save_hardware_profile(&mk(Backend::Cpu)).unwrap();
        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM hardware_profile", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "hardware_profile 必须是单行表");
        let sel: String = db
            .conn()
            .query_row("SELECT backend_selected FROM hardware_profile", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(sel, "cpu", "应被最后一次写入覆盖");
    }

    #[test]
    fn now_ms_is_plausible() {
        // 2020-01-01 之后
        assert!(now_ms() > 1_577_836_800_000);
    }
}
