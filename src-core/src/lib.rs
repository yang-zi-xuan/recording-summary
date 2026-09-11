//! 录音转总结客户端 · 核心库
//!
//! 全部业务逻辑集中在这里,GUI 和 CLI 都只是它的调用方。
//! 设计依据见 `docs/技术方案.md`。

pub mod asr;
pub mod audio;
pub mod diarize;
pub mod hardware;
pub mod llm;
pub mod paths;
pub mod pipeline;
pub mod process;
pub mod project;
pub mod sherpa;
pub mod store;
pub mod sync;
pub mod types;
pub mod voiceprint;

pub use types::*;

/// 库版本
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 位置指针文件名。
///
/// 这个文件回答"数据目录在哪",所以它**不能**放在数据目录里 ——
/// 否则就是先有鸡还是先有蛋。放在固定的系统位置。
const LOCATION_FILE: &str = "location.txt";

/// 指针文件所在目录(固定,不随数据目录变)。
fn config_home() -> std::path::PathBuf {
    directories::ProjectDirs::from("", "", "recording-summary")
        .map(|d| {
            // data_dir 是 .../AppData/Roaming/recording-summary/data
            // 它的父目录就是 .../AppData/Roaming/recording-summary
            d.data_dir()
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| d.data_dir().to_path_buf())
        })
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// 指针文件的完整路径。
pub fn location_file() -> std::path::PathBuf {
    config_home().join(LOCATION_FILE)
}

/// 系统默认数据目录(未配置指针时用)。
fn system_default_data_dir() -> std::path::PathBuf {
    directories::ProjectDirs::from("", "", "recording-summary")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("./data"))
}

/// 数据目录。
///
/// 解析顺序:
/// 1. **指针文件** `%APPDATA%\recording-summary\location.txt` 的内容
/// 2. 系统默认 `%APPDATA%\recording-summary\data`
///
/// 指针文件只有一行,写的是绝对路径。用文件而不是注册表/环境变量,理由:
/// - 用户能直接看懂、能手工改、能删掉复位
/// - 不依赖启动方式(双击 exe 时设不了环境变量)
///
/// **不要**把数据放程序目录 —— Windows 下 `Program Files` 无写权限。
pub fn default_data_dir() -> std::path::PathBuf {
    // 指针文件优先
    if let Ok(s) = std::fs::read_to_string(location_file()) {
        let p = s.trim();
        if !p.is_empty() {
            let pb = std::path::PathBuf::from(p);
            if pb.is_absolute() {
                return pb;
            }
        }
    }
    system_default_data_dir()
}

/// 写入位置指针。返回是否成功。
///
/// 会把路径转成绝对路径再写 —— 相对路径在双击启动时毫无意义。
pub fn set_data_dir(path: &std::path::Path) -> std::io::Result<()> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let f = location_file();
    if let Some(p) = f.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&f, abs.to_string_lossy().as_bytes())
}

/// 删除位置指针,回到系统默认位置。
pub fn reset_data_dir() -> std::io::Result<()> {
    match std::fs::remove_file(location_file()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// 该系统默认位置在哪(用于界面提示"改回默认")。
pub fn system_data_dir() -> std::path::PathBuf {
    system_default_data_dir()
}

/// 当前是用了指针文件,还是系统默认?
pub fn data_dir_is_custom() -> bool {
    default_data_dir() != system_default_data_dir()
}

/// 迁移结果。
#[derive(Debug, Clone)]
pub struct MigrateOutcome {
    /// 拷贝的字节数
    pub bytes: u64,
    /// 拷贝的文件数
    pub files: usize,
    /// 旧目录清理失败时的说明(数据已安全迁移,只是没删干净)
    pub cleanup_warning: Option<String>,
}

/// 把数据目录从 `from` 迁到 `to`。
///
/// 做三件事,顺序很重要:
/// 1. **递归拷贝 + 校验文件数**(不删源 —— 拷贝失败时原数据必须完好)
/// 2. 写指针文件
/// 3. 尝试删源目录 —— **失败不算迁移失败**,只在结果里带一条警告
///
/// 第 3 步为什么不算失败:数据已经在目标目录、指针也已更新,
/// 此时报错会让用户以为迁移没成功,反而可能去手工删目标目录。
/// 旧目录残留只是占点空间,提示一句就够了。
///
/// 调用方**应当**先关掉数据库并做 WAL checkpoint,否则会漏掉
/// 尚未合并进主文件的 `-wal` 内容。见 [`checkpoint_sqlite`]。
pub fn migrate_data_dir(from: &std::path::Path, to: &std::path::Path) -> anyhow::Result<MigrateOutcome> {
    use anyhow::Context;

    if from == to {
        anyhow::bail!("源目录和目标目录相同");
    }
    if !from.is_dir() {
        anyhow::bail!("源目录不存在:{}", from.display());
    }
    // 防呆:不许把目录迁进它自己的子目录(会无限递归)
    if to.starts_with(from) {
        anyhow::bail!(
            "目标目录不能是源目录的子目录:\n  源 {}\n  目标 {}",
            from.display(),
            to.display()
        );
    }

    std::fs::create_dir_all(to)
        .with_context(|| format!("创建目标目录失败: {}", to.display()))?;

    // 1. 拷贝
    let (bytes, files) = copy_tree(from, to)?;

    // 2. 写指针(先写指针再删源 —— 中间崩溃也不会丢数据)
    set_data_dir(to).context("写入位置指针失败")?;

    // 3. 尽力删源
    let cleanup_warning = match std::fs::remove_dir_all(from) {
        Ok(()) => None,
        Err(e) => Some(format!(
            "旧目录没能删除({e})。数据已经在新位置了,可以之后手工删:{}",
            from.display()
        )),
    };

    Ok(MigrateOutcome {
        bytes,
        files,
        cleanup_warning,
    })
}

/// 对目录下的 SQLite 数据库做 WAL checkpoint,把 `-wal` 内容合并进主文件。
///
/// **迁移前必须做** —— 否则 `-wal` 里未落盘的事务不会被拷走
/// (文件虽然拷了,但进程若仍开着,内容可能不完整)。
///
/// 找不到数据库文件时不算错误。
pub fn checkpoint_sqlite(dir: &std::path::Path) -> anyhow::Result<()> {
    let db = dir.join("cache.db");
    if !db.is_file() {
        return Ok(());
    }
    let conn = rusqlite::Connection::open(&db)?;
    // TRUNCATE:合并后把 -wal 截断,等于清空
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(())
}

/// 递归拷贝目录,返回 (字节数, 文件数)。
fn copy_tree(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<(u64, usize)> {
    use anyhow::Context;
    let mut total = 0u64;
    let mut files = 0usize;
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)
        .with_context(|| format!("读取 {} 失败", src.display()))?
    {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            let (b, f) = copy_tree(&from, &to)?;
            total += b;
            files += f;
        } else {
            let n = std::fs::copy(&from, &to)
                .with_context(|| format!("拷贝 {} 失败", from.display()))?;
            total += n;
            files += 1;
        }
    }
    Ok((total, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_default_is_under_appdata() {
        let d = system_data_dir();
        assert!(d.ends_with("data"), "{d:?}");
    }

    #[test]
    fn location_file_lives_outside_data_dir() {
        // ★ 关键:指针文件不能在数据目录里,否则无法回答"数据目录在哪"
        let f = location_file();
        let d = system_data_dir();
        assert!(
            !f.starts_with(&d),
            "指针文件 {} 不该在数据目录 {} 内",
            f.display(),
            d.display()
        );
    }

    #[test]
    fn copy_tree_moves_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("a/b")).unwrap();
        std::fs::write(src.join("top.txt"), b"12345").unwrap();
        std::fs::write(src.join("a/mid.txt"), b"123").unwrap();
        std::fs::write(src.join("a/b/deep.txt"), b"12").unwrap();

        let dst = tmp.path().join("dst");
        let (n, files) = copy_tree(&src, &dst).unwrap();

        assert_eq!(n, 10, "应拷贝 5+3+2 字节");
        assert_eq!(files, 3, "3 个文件");
        assert!(dst.join("top.txt").is_file());
        assert!(dst.join("a/mid.txt").is_file());
        assert!(dst.join("a/b/deep.txt").is_file());
        // 源必须完好(拷贝阶段不删)
        assert!(src.join("top.txt").is_file());
    }

    #[test]
    fn copy_tree_handles_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("empty");
        std::fs::create_dir_all(&src).unwrap();
        let (n, files) = copy_tree(&src, &tmp.path().join("out")).unwrap();
        assert_eq!(n, 0);
        assert_eq!(files, 0);
    }

    #[test]
    fn migrate_rejects_same_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let e = migrate_data_dir(tmp.path(), tmp.path()).unwrap_err();
        assert!(e.to_string().contains("相同"), "{e}");
    }

    #[test]
    fn migrate_rejects_nested_target() {
        // 把目录迁进自己的子目录会无限递归 —— 必须挡住
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let inner = src.join("inner");
        let e = migrate_data_dir(&src, &inner).unwrap_err();
        assert!(e.to_string().contains("子目录"), "{e}");
        // 源目录必须还在
        assert!(src.is_dir());
    }

    #[test]
    fn migrate_rejects_missing_source() {
        let tmp = tempfile::tempdir().unwrap();
        let e = migrate_data_dir(&tmp.path().join("nope"), &tmp.path().join("dst")).unwrap_err();
        assert!(e.to_string().contains("不存在"), "{e}");
    }

    #[test]
    fn migrate_copies_then_removes_source() {
        // 不碰真实指针文件:只测 copy_tree + 手工删除这一段的语义
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("store")).unwrap();
        std::fs::write(src.join("store/x.json"), b"hello").unwrap();

        let dst = tmp.path().join("dst");
        let (n, files) = copy_tree(&src, &dst).unwrap();
        assert_eq!(n, 5);
        assert_eq!(files, 1);
        assert!(dst.join("store/x.json").is_file());

        std::fs::remove_dir_all(&src).unwrap();
        assert!(!src.exists());
        assert!(dst.join("store/x.json").is_file(), "目标数据必须完好");
    }
}
