//! 验证资源定位在各种工作目录下都能找到文件。
//!
//! 背景:`ui/vendor/mermaid.min.js` 用裸相对路径查找时,
//! 双击 exe 启动(工作目录 = C:\Windows)会报"未找到"。
//!
//! 运行:
//! ```text
//! cargo run -p rs-core --example check_assets
//! ```

use rs_core::paths;

fn main() {
    println!("当前工作目录: {:?}", std::env::current_dir().ok());
    println!("可执行文件  : {:?}", std::env::current_exe().ok());
    println!();

    let data_dir = rs_core::default_data_dir();
    println!("数据目录    : {}", data_dir.display());
    println!();

    let assets: &[(&str, bool)] = &[
        ("ui/vendor/mermaid.min.js", false), // 必需(导图渲染)
        ("models/ggml-large-v3-turbo.bin", false),
        ("binaries/cuda/whisper-cli.exe", false),
        ("binaries/cpu/whisper-cli.exe", false),
    ];

    let mut missing = Vec::new();
    for (rel, _required) in assets {
        match paths::find_asset_file(rel, &data_dir) {
            Some(p) => {
                let kb = std::fs::metadata(&p).map(|m| m.len() / 1024).unwrap_or(0);
                println!("  ✅ {rel:<40} {kb:>8} KB");
                println!("       → {}", p.display());
            }
            None => {
                println!("  ❌ {rel:<40} 未找到");
                missing.push(*rel);
            }
        }
    }

    println!();
    if missing.is_empty() {
        println!("全部就位。");
    } else {
        println!("缺失 {} 项:", missing.len());
        for m in missing {
            println!("  - {m}");
        }
        std::process::exit(1);
    }
}
