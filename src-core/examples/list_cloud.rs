//! 列出云端目录树,确认有什么可以拉下来。
//!
//! ```text
//! cargo run -p rs-core --example list_cloud
//! ```

use rs_core::sync::{WebDavClient, WebDavConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::var("DIAG_URL").unwrap_or_default();
    let user = std::env::var("DIAG_USER").unwrap_or_default();
    let pass = std::env::var("DIAG_PASS").unwrap_or_default();
    let dir = std::env::var("DIAG_DIR").unwrap_or_else(|_| "recording-summary".into());
    if url.is_empty() || user.is_empty() || pass.is_empty() {
        eprintln!("需要 DIAG_URL / DIAG_USER / DIAG_PASS");
        std::process::exit(2);
    }

    let client = WebDavClient::new(WebDavConfig {
        base_url: url,
        username: user,
        password: pass,
        remote_dir: dir,
        timeout_secs: 60,
    })?;

    println!("远端根: {}\n", client.config().root());

    // 逐层列出,带大小
    async fn walk(client: &WebDavClient, rel: &str, depth: usize, max: usize) {
        if depth > max {
            return;
        }
        let Ok(nodes) = client.list_nodes(rel).await else {
            return;
        };
        for n in nodes {
            let pad = "  ".repeat(depth);
            if n.is_dir {
                println!("{pad}[DIR]  {}", n.rel_path);
                Box::pin(walk(client, &n.rel_path, depth + 1, max)).await;
            } else {
                let mb = n.size.unwrap_or(0) as f64 / 1024.0 / 1024.0;
                println!("{pad}[FILE] {:<64} {:>9.2} MB", n.rel_path, mb);
            }
        }
    }

    walk(&client, "", 0, 4).await;
    Ok(())
}
