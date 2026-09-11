//! 本地存储。
//!
//! 核心设计(技术方案 §5.1):**分享的是文件,不是数据库。**
//!
//! ```text
//! 共享数据(文件形态,跨设备同步)      本地数据(SQLite,不共享)
//!   transcript/ab/ab3f9c...json        session 索引与元数据
//!   summary/ab/ab3f9c...md             转写缓存 + 说话人区分缓存
//!   speaker_labels/*.json             硬件画像(本机硬件)
//!   profiles.json                     speaker_embeddings(供声纹登记)
//!   manifest.json
//! ```
//!
//! 数据库只做本地缓存,**可以随时删掉从文件重建** —— 于是完全不需要处理
//! 数据库合并冲突,这是同步系统里最麻烦的部分。

pub mod cache;
pub mod db;
pub mod files;

pub use cache::{CacheKeyInputs, DiarizeCacheKey, TranscriptCacheKey};
pub use db::Db;
pub use files::FileStore;
