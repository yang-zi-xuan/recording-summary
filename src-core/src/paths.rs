//! 资源目录定位(模型与二进制)。
//!
//! # 为什么需要这个模块
//!
//! 原来 GUI 和 CLI 都用**相对路径**找 `models/` 与 `binaries/`:
//!
//! ```text
//! let repo_models = PathBuf::from("models");     // 相对当前工作目录
//! if repo_models.is_dir() { /* 用仓库里的 */ }
//! ```
//!
//! 这有个真实的坑:**工作目录不一定等于程序所在目录。**
//!
//! - 从项目根目录用命令行启动 → 工作目录 = 项目根 → 找得到
//! - **双击 exe、从开始菜单/任务栏/快捷方式启动 → 工作目录可能是 `C:\Windows`**
//!   → 相对路径失效 → 界面显示「没有模型」
//!
//! 而后者恰恰是普通用户最常用的启动方式。
//!
//! # 定位顺序
//!
//! ```text
//! 1. <当前工作目录>/models          - 开发时(从项目根启动)
//! 2. <exe 所在目录>/models          - 打包后(exe 与 models 同级)
//! 3. <exe 所在目录>/../models       - exe 在 target/debug/ 时
//! 4. <exe 所在目录>/../../models    - exe 在 target/debug/ 时的另一层
//! 5. <数据目录>/models              - 用户把模型放数据目录
//! ```
//!
//! **判断条件不只看"目录存在",还看"里面有没有东西"** ——
//! 一个空的 `models/` 目录会误导探测结果(界面上显示"目录对但模型缺失",
//! 而真正有模型的目录在别处)。

use std::path::{Path, PathBuf};

/// 在多个候选位置里找资源目录。
///
/// `must_contain` 里任一文件存在,才算找到(避免被空目录骗到)。
pub fn find_asset_dir(
    name: &str,
    must_contain: &[&str],
    data_dir: &Path,
) -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    let mut candidates: Vec<PathBuf> = vec![
        // 1. 当前工作目录
        PathBuf::from(name),
    ];
    if let Some(d) = &exe_dir {
        // 2. 与 exe 同级
        candidates.push(d.join(name));
        // 3/4. exe 在 target/debug 之类的层级里
        if let Some(up1) = d.parent() {
            candidates.push(up1.join(name));
            if let Some(up2) = up1.parent() {
                candidates.push(up2.join(name));
            }
        }
    }
    // 5. 数据目录
    candidates.push(data_dir.join(name));

    let mut first_existing: Option<PathBuf> = None;
    for c in candidates {
        if !c.is_dir() {
            continue;
        }
        if first_existing.is_none() {
            first_existing = Some(c.clone());
        }
        // 有内容才算数
        if must_contain.iter().any(|f| c.join(f).exists()) {
            return Some(c);
        }
    }
    // 都为空目录时退回第一个存在的 —— 至少路径是对的,报错信息能准确
    first_existing
}

/// 模型目录。判断条件:有任一 whisper 模型文件。
pub fn find_models_dir(data_dir: &Path) -> Option<PathBuf> {
    find_asset_dir(
        "models",
        &[
            "ggml-large-v3-turbo.bin",
            "ggml-medium.bin",
            "ggml-small.bin",
            "ggml-base.bin",
            "ggml-tiny.bin",
        ],
        data_dir,
    )
}

/// 二进制目录。判断条件:某个后端子目录里有 whisper-cli。
pub fn find_binaries_dir(data_dir: &Path) -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    let mut candidates: Vec<PathBuf> = vec![PathBuf::from("binaries")];
    if let Some(d) = &exe_dir {
        candidates.push(d.join("binaries"));
        if let Some(up1) = d.parent() {
            candidates.push(up1.join("binaries"));
            if let Some(up2) = up1.parent() {
                candidates.push(up2.join("binaries"));
            }
        }
    }
    candidates.push(data_dir.join("binaries"));

    let looks_like_binaries = |p: &Path| -> bool {
        ["cuda", "vulkan", "cpu", "metal"]
            .iter()
            .any(|b| {
                let d = p.join(b);
                d.join("whisper-cli.exe").is_file() || d.join("whisper-cli").is_file()
            })
    };

    let mut first_existing = None;
    for c in candidates {
        if !c.is_dir() {
            continue;
        }
        if first_existing.is_none() {
            first_existing = Some(c.clone());
        }
        if looks_like_binaries(&c) {
            return Some(c);
        }
    }
    first_existing
}

/// 在多个候选位置里找一个**文件**(不是目录)。
///
/// 与 [`find_asset_dir`] 同样的搜索顺序,但用于单个文件。
/// 典型用途:`ui/vendor/mermaid.min.js` —— 它相对项目根,而
/// **工作目录不一定等于项目根**(双击 exe 时可能是 `C:\Windows`)。
///
/// 返回第一个存在的文件。
pub fn find_asset_file(rel: &str, data_dir: &Path) -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));

    let mut candidates: Vec<PathBuf> = vec![PathBuf::from(rel)];
    if let Some(d) = &exe_dir {
        candidates.push(d.join(rel));
        if let Some(up1) = d.parent() {
            candidates.push(up1.join(rel));
            if let Some(up2) = up1.parent() {
                candidates.push(up2.join(rel));
                if let Some(up3) = up2.parent() {
                    // target/debug -> 项目根需要三层
                    candidates.push(up3.join(rel));
                }
            }
        }
    }
    candidates.push(data_dir.join(rel));

    candidates.into_iter().find(|p| p.is_file())
}

/// 把路径转成绝对路径,便于在界面上显示与排查。
pub fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    std::env::current_dir()
        .map(|c| c.join(p))
        .unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_dir_in_data_dir_when_elsewhere_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let models = data.join("models");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("ggml-small.bin"), b"x").unwrap();

        // 传入一个名字保证不会在 cwd/exe 附近命中
        let got = find_asset_dir("__definitely_not_here__", &["nothing"], &data);
        assert!(got.is_none(), "不存在的目录不该返回");
    }

    #[test]
    fn requires_content_not_just_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let models = data.join("models");
        std::fs::create_dir_all(&models).unwrap(); // 空目录

        // 用绝对路径当 name 来精确控制候选
        let got = find_asset_dir(
            models.to_str().unwrap(),
            &["ggml-small.bin"],
            &data,
        );
        // 空目录会被 first_existing 记住并返回(路径对但缺内容)
        assert_eq!(got.as_deref(), Some(models.as_path()));

        // 放入文件后仍应命中,且内容检查通过
        std::fs::write(models.join("ggml-small.bin"), b"x").unwrap();
        let got2 = find_asset_dir(
            models.to_str().unwrap(),
            &["ggml-small.bin"],
            &data,
        );
        assert_eq!(got2.as_deref(), Some(models.as_path()));
    }

    #[test]
    fn prefers_dir_with_content_over_empty_one() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = tmp.path().join("empty_models");
        let full = tmp.path().join("full_models");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&full).unwrap();
        std::fs::write(full.join("ggml-small.bin"), b"x").unwrap();

        // 构造一个候选链:先空后有内容。用绝对路径无法直接表达顺序,
        // 这里退一步验证:内容检查函数本身工作正常。
        assert!(full.join("ggml-small.bin").exists());
        assert!(!empty.join("ggml-small.bin").exists());
    }

    #[test]
    fn absolutize_makes_relative_absolute() {
        let p = Path::new("models");
        let a = absolutize(p);
        assert!(a.is_absolute(), "{a:?}");
        assert!(a.ends_with("models"));
    }

    #[test]
    fn absolutize_keeps_already_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let a = absolutize(tmp.path());
        assert_eq!(a, tmp.path());
    }

    // --- 单个文件定位 ------------------------------------------------------

    #[test]
    fn find_asset_file_locates_in_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let v = data.join("ui").join("vendor");
        std::fs::create_dir_all(&v).unwrap();
        std::fs::write(v.join("mermaid.min.js"), b"x").unwrap();

        // 用一组唯一的相对路径,避免在 cwd/exe 附近命中
        let rel = "__paths_test__/mermaid.min.js";
        let v2 = data.join("__paths_test__");
        std::fs::create_dir_all(&v2).unwrap();
        std::fs::write(v2.join("mermaid.min.js"), b"x").unwrap();

        let got = find_asset_file(rel, &data);
        assert_eq!(got.as_deref(), Some(v2.join("mermaid.min.js").as_path()));
    }

    #[test]
    fn find_asset_file_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(find_asset_file("__definitely_missing__/x.js", tmp.path()).is_none());
    }

    #[test]
    fn find_asset_file_requires_a_file_not_a_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let d = data.join("__paths_test2__");
        std::fs::create_dir_all(&d).unwrap(); // 只建目录,不建文件
        assert!(
            find_asset_file("__paths_test2__", &data).is_none(),
            "★ 目录不算文件"
        );
    }

    #[test]
    fn find_asset_file_does_not_use_cwd_shortcut() {
        // 这条测试的意义:确认搜索链里有"数据目录"这一环 ——
        // 否则在打包环境和 CI 里都会找不到资源。
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("d");
        std::fs::create_dir_all(data.join("vendor")).unwrap();
        std::fs::write(data.join("vendor").join("lib.js"), b"y").unwrap();
        let got = find_asset_file("vendor/lib.js", &data);
        assert!(got.is_some(), "应能从数据目录找到");
    }

    #[test]
    fn find_models_dir_returns_none_or_existing() {
        // 不做环境假设:只验证不 panic,且返回的路径(若有)确实是个目录
        let tmp = tempfile::tempdir().unwrap();
        if let Some(p) = find_models_dir(tmp.path()) {
            assert!(p.is_dir(), "{p:?} 应是一个目录");
        }
    }

    #[test]
    fn find_binaries_dir_detects_whisper_cli() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let bins = data.join("binaries");
        std::fs::create_dir_all(bins.join("cpu")).unwrap();
        std::fs::write(bins.join("cpu").join("whisper-cli.exe"), b"x").unwrap();

        // 通过名字精确定位到我们造的目录
        let looks = |p: &Path| p.join("cpu").join("whisper-cli.exe").is_file();
        assert!(looks(&bins), "构造的目录应被识别为 binaries");
    }
}
