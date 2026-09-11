//! 同步连通性诊断。
//!
//! 连接失败时,把"到底哪一步不对"拆开来看:分别探测**服务根**与
//! **远端子目录**,输出每一步的 HTTP 状态。只发 PROPFIND(读),
//! **不修改任何数据**。
//!
//! 运行:
//! ```text
//! cargo run -p rs-core --example diag_sync
//! ```
//!
//! 凭据从**环境变量**读取(与 `rs sync` 命令一致),
//! 刻意**不**提供"从凭据管理器读密码并打印"的路径 ——
//! 程序里不该存在能把密码读出来的代码。

use rs_core::sync::{WebDavClient, WebDavConfig};
use std::io::Write;

fn main() -> anyhow::Result<()> {
    let base =
        std::env::var("DIAG_URL").unwrap_or_else(|_| "https://cloud.example.com/seafdav".into());
    let user = std::env::var("DIAG_USER").unwrap_or_default();
    let pass = std::env::var("DIAG_PASS").unwrap_or_default();
    let dir = std::env::var("DIAG_DIR").unwrap_or_default();

    if user.is_empty() {
        eprintln!("用法:");
        eprintln!("  set DIAG_USER=账号@auth.local");
        eprintln!("  set DIAG_PASS=密码");
        eprintln!("  set DIAG_DIR=recording-summary      (可选)");
        eprintln!("  cargo run -p rs-core --example diag_sync");
        std::process::exit(2);
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    println!("用户名  : {user}");
    println!("密码长度: {} 字符", pass.chars().count());
    // 这些是真实会踩的坑,值得一提
    if pass.is_empty() {
        println!("  ❌ 密码为空 —— 这必然 401");
    }
    if pass != pass.trim() {
        println!("  ⚠ 密码首尾有空白字符 —— 会导致认证失败");
    }
    if pass.chars().any(|c| c == '\n' || c == '\r') {
        println!("  ⚠ 密码含换行符 —— 会导致认证失败");
    }
    println!();

    let mut cases: Vec<(&str, String)> = vec![("服务根", String::new())];
    if !dir.is_empty() {
        cases.push(("远端子目录", dir.clone()));
    }

    for (label, sub) in cases {
        let cfg = WebDavConfig {
            base_url: base.clone(),
            username: user.clone(),
            password: pass.clone(),
            remote_dir: sub.clone(),
            timeout_secs: 30,
        };
        let client = WebDavClient::new(cfg)?;
        println!("── {label} ──");
        println!("  URL: {}", client.config().root());
        std::io::stdout().flush()?;

        match rt.block_on(client.list("")) {
            Ok(v) => {
                println!("  ✅ HTTP 207,可列出 {} 项", v.len());
                for (p, _) in v.iter().take(10) {
                    println!("       {p}");
                }
            }
            Err(e) => {
                let s = e.to_string();
                println!("  ❌ {}", s.lines().next().unwrap_or(""));
                if s.contains("401") {
                    println!("     → 服务端拒绝了凭据。注意:用户名错和密码错都返回 401,");
                    println!("       无法从状态码区分 —— 但服务根也 401 说明凭据本身有问题。");
                } else if s.contains("404") {
                    println!("     → 路径不存在。认证可能已通过,只是这个目录还没建。");
                }
            }
        }
        println!();
    }

    Ok(())
}
