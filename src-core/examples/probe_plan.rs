//! 打印同步计划的每一项,看清"跳过"到底是什么。
//!
//! ```text
//! cargo run -p rs-core --example probe_plan
//! ```

use rs_core::store::files::FileStore;
use rs_core::sync::{manifest::Manifest, selection::*, Syncer, WebDavClient, WebDavConfig};

// ⚠ 这里**不能**用 `#[tokio::main]`。
//
// `Syncer` 内部用 `block_on` 驱动自己的运行时;如果 main 已经在一个
// tokio 运行时里,那个 `block_on` 会 panic("Cannot start a runtime
// from within a runtime")。所以 main 保持同步,只在拉远端 manifest
// 那一处临时建一个运行时。
fn main() -> anyhow::Result<()> {
    let url = std::env::var("DIAG_URL").unwrap_or_default();
    let user = std::env::var("DIAG_USER").unwrap_or_default();
    let pass = std::env::var("DIAG_PASS").unwrap_or_default();
    let dir = std::env::var("DIAG_DIR").unwrap_or_else(|_| "recording-summary".into());
    let data = std::env::var("RECSUM_DATA_DIR")
        .unwrap_or_else(|_| "D:\\RecordingSummary".into());
    if url.is_empty() || user.is_empty() || pass.is_empty() {
        eprintln!("需要 DIAG_URL / DIAG_USER / DIAG_PASS");
        std::process::exit(2);
    }

    let files = FileStore::new(std::path::Path::new(&data).join("store"));
    let cfg = WebDavConfig {
        base_url: url,
        username: user,
        password: pass,
        remote_dir: dir,
        timeout_secs: 60,
    };

    let manifest = Manifest::load_or_new(&files.manifest_path())?;
    println!("manifest 条目: {}", manifest.live_files().count());

    let cfg2 = cfg.clone();
    let syncer = Syncer::new(cfg, &files)?.with_selection(SyncSelection::all());
    let plan = syncer.plan(&manifest)?;
    println!("计划项: {}\n", plan.len());

    let mut up = 0;
    let mut down = 0;
    let mut skip = 0;
    let mut del = 0;

    for it in &plan {
        let abs = files.root().join(&it.rel_path);
        let local = abs.exists();
        let action = match it.action {
            rs_core::sync::Action::Upload => {
                up += 1;
                "上传"
            }
            rs_core::sync::Action::Download => {
                down += 1;
                "下载"
            }
            rs_core::sync::Action::Skip => {
                skip += 1;
                "跳过"
            }
            rs_core::sync::Action::DeleteRemote => {
                del += 1;
                "删云端"
            }
        };
        println!(
            "  {action:<6} 本地存在={:<5} size={:>10} etag={:<12} {}",
            local,
            it.local_size,
            it.remote_etag.as_deref().unwrap_or("-"),
            it.rel_path
        );
    }

    println!("\n合计: 上传 {up} / 下载 {down} / 跳过 {skip} / 删云端 {del}");

    // ★ 复现 run() 的完整前置步骤:先合并远端 manifest,再算计划。
    //   run() 就是这么做事的,所以它看到的计划应该和上面一致 ——
    //   不一致就说明合并改了东西。
    println!("\n=== 模拟 run():先合并远端 manifest 再算计划 ===");
    let c = WebDavClient::new(cfg2)?;
    // 用一个独立的一次性运行时拉远端 manifest —— 不能借用 Syncer 内部那个
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let remote_bytes = rt.block_on(async { c.get("manifest.json").await });
    match remote_bytes {
        Ok(Some(b)) => match Manifest::from_bytes(&b) {
            Ok(rm) => {
                let mut m2 = manifest.clone();
                println!("  远端 manifest: {} 条", rm.live_files().count());
                m2.merge_from(&rm);
                println!("  合并后:        {} 条", m2.live_files().count());
                match syncer.plan(&m2) {
                    Ok(p2) => {
                        let mut d2 = 0;
                        let mut s2 = 0;
                        for it in &p2 {
                            match it.action {
                                rs_core::sync::Action::Download => {
                                    d2 += 1;
                                    println!("    下载 {}", it.rel_path);
                                }
                                rs_core::sync::Action::Skip => s2 += 1,
                                _ => {}
                            }
                        }
                        println!("  合并后计划: {} 项(下载 {d2} / 跳过 {s2})", p2.len());
                    }
                    Err(e) => println!("  计划失败: {e}"),
                }
            }
            Err(e) => println!("  远端 manifest 解析失败: {e}"),
        },
        Ok(None) => println!("  云端没有 manifest.json"),
        Err(e) => println!("  读不到远端 manifest: {e}"),
    }

    Ok(())
}
