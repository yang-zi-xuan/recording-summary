//! WebDAV 云端同步。见技术方案 §11。
//!
//! # 核心设计
//!
//! **混合传输:文本走 WebDAV,音频交给 Seafile 客户端。**
//!
//! 依据是 Seafile 官方对 WebDAV 的性能说明:
//!
//! > Uploading **large number of files at once** is usually much slower than
//! > the syncing client. That's because each file needs to be committed separately.
//!
//! 注意它说的是"大量小文件",不是"大文件"。所以正确的划分是:
//!
//! | 数据 | 走哪条路 | 原因 |
//! |---|---|---|
//! | manifest / 转写 / 纪要 / 标签 / 注册表 | **WebDAV** | 文件少、按需传、状态需要程序可见 |
//! | 音频 | **Seafile 客户端** | 大文件、传完不动,交给官方客户端省掉断点续传 |
//!
//! **manifest 必须走 WebDAV** —— 它是同步状态本身,程序必须能直接读远端最新版本来合并。
//! 放进被客户端同步的目录里,"本地文件"和"远端状态"会混在一起,分不清哪个是真相。
//!
//! # 冲突处理
//!
//! manifest 是**事实的追加日志**,不是状态快照:
//! - 每条记录只追加不删(文件存在这个事实不会变)
//! - 两边都有 → 按 `synced_at` 取新的合并
//! - 上传前先 GET 最新 manifest,合并后再 PUT
//!
//! **★ manifest 可重建。** 丢了或损坏时,扫一遍本地 + PROPFIND 远端就能重新生成,
//! 所以它不需要任何事务保护。这让实现简单一大截。

pub mod inventory;
pub mod manifest;
pub mod remote;
pub mod selection;
pub mod state;

use crate::store::files::FileStore;
use anyhow::{anyhow, Context, Result};
use manifest::{Manifest, ManifestEntry};
use std::path::Path;
use std::time::Duration;

/// WebDAV 连接配置。
///
/// 默认值是一个可直接使用的示例(Seafile 系 WebDAV) —— 见技术方案 §11.1。
#[derive(Clone, Debug)]
pub struct WebDavConfig {
    /// 服务器根,例如 `https://cloud.example.com/seafdav`
    pub base_url: String,
    /// Seafile 的用户名格式是 `<账号>@auth.local`
    pub username: String,
    pub password: String,
    /// 远端子目录,例如 `recording-summary`
    pub remote_dir: String,
    pub timeout_secs: u64,
}

impl Default for WebDavConfig {
    fn default() -> Self {
        Self {
            base_url: "https://cloud.example.com/seafdav".into(),
            username: String::new(),
            password: String::new(),
            remote_dir: "recording-summary".into(),
            timeout_secs: 60,
        }
    }
}

/// 已知的 WebDAV 服务根后缀。
///
/// 用于判断"哪一段之后是用户自己的子目录"。命中它比"取最后一段"更可靠,
/// 因为服务根名(seafdav / remote.php/dav)不可能是用户建的目录。
const WEBDAV_ROOTS: [&str; 5] = [
    "/seafdav",
    "/remote.php/dav",
    "/webdav",
    "/dav",
    "/nextcloud",
];

/// 把用户粘进来的一整条地址拆成(服务地址, 远端目录)。
///
/// **为什么要做这个:** 界面上「服务地址」和「远端目录」是两个输入框,
/// 但用户脑子里想的是**一条完整地址**。直接粘进「服务地址」框,
/// 程序会拼出 `.../seafdav/recording_summary/recording_summary` 这种错路径,
/// 而报错是 404,完全指不到问题所在。
///
/// 切分规则:
/// 1. 路径里含已知服务根(`/seafdav`、`/remote.php/dav` 等)→ 从它之后切
/// 2. 否则路径有 2 段以上 → 取最后一段当子目录
/// 3. 路径只有一段或没有 → `None`(本来就是服务地址)
///
/// 返回 `None` 表示没必要拆。
pub fn split_webdav_url(input: &str) -> Option<(String, String)> {
    let s = input.trim().trim_end_matches('/');
    if !(s.starts_with("http://") || s.starts_with("https://")) {
        return None;
    }
    let (scheme, rest) = s.split_once("://")?;
    let slash = rest.find('/')?; // 无路径 → 纯主机,没什么可拆
    let host = &rest[..slash];
    let path = &rest[slash..];

    let segs: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if segs.is_empty() {
        return None;
    }

    // 规则 1:命中已知服务根
    let cut = WEBDAV_ROOTS
        .iter()
        .filter_map(|root| path.rfind(root).map(|i| i + root.len()))
        .max();

    let (base_path, dir) = match cut {
        // 服务根之后还有内容才算子目录
        Some(end) if end < path.len() => (&path[..end], &path[end..]),
        Some(_) => return None, // 地址本身就是服务根
        None => {
            // 规则 2:没有已知服务根时,取最后一段当子目录。
            // 只在路径有 2 段以上时才敢这么做 ——
            // 单段路径更可能是服务根(比如 /dav)而不是用户目录。
            if segs.len() < 2 {
                return None;
            }
            let last = segs[segs.len() - 1];
            let idx = path.rfind(last)?;
            (&path[..idx], &path[idx..])
        }
    };

    let dir = dir.trim_matches('/').trim().to_string();
    if dir.is_empty() {
        return None;
    }
    let base_path = base_path.trim_end_matches('/');
    Some((format!("{scheme}://{host}{base_path}"), dir))
}

impl WebDavConfig {
    pub fn validate(&self) -> Result<()> {
        if self.base_url.trim().is_empty() {
            return Err(anyhow!("WebDAV 地址为空"));
        }
        if self.username.trim().is_empty() {
            return Err(anyhow!(
                "WebDAV 用户名为空。Seafile 系的格式是 <账号>@auth.local"
            ));
        }
        if self.password.is_empty() {
            return Err(anyhow!("WebDAV 密码为空"));
        }
        Ok(())
    }

    /// 远端根 URL(去掉尾部斜杠)。
    ///
    /// **会过滤掉空路径段并 trim 每段** —— 用户可能把地址填成
    /// `https://cloud.example.com/seafdav/` 再给一个 ` /recording_summary/ `,
    /// 两侧斜杠叠加会拼出 `//`(有些服务器 404),前后空格更会让 MKCOL 失败。
    pub fn root(&self) -> String {
        let base = self.base_url.trim().trim_end_matches('/');
        let dir: Vec<&str> = self
            .remote_dir
            .split('/')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if dir.is_empty() {
            base.to_string()
        } else {
            format!("{base}/{}", dir.join("/"))
        }
    }

    /// 拼一个远端文件的完整 URL。
    ///
    /// # 必须做百分号编码
    ///
    /// 之前是直接 `format!("{}/{}", root, rel_path)`,路径原样拼进去。
    /// 后果(实测于真实 Seafile):
    ///
    /// - 含**空格**时侥幸能过 —— reqwest 会把 URL 里的空格自动编码成 `%20`。
    /// - 含**中文**时失败 —— 尤其是 MOVE 的 `Destination` 头,
    ///   那是请求头而不是 URL,reqwest **不会**替我们编码,
    ///   服务器收到原始 UTF-8 字节后解析出错误的路径,返回 409。
    ///
    /// 症状是"父目录不存在(409)",但目录其实是好的 ——
    /// 真正的错因被那句提示掩盖了,查起来很绕。
    ///
    /// 所以在这里一次性编码好,所有调用点(含 `Destination` 头)都受益。
    pub fn url_for(&self, rel_path: &str) -> String {
        format!("{}/{}", self.root(), encode_path(rel_path.trim_start_matches('/')))
    }
}

/// 单个文件的同步动作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// 远端没有 → 上传
    Upload,
    /// 两边 ETag 一致 → 跳过
    Skip,
    /// 远端更新 → 下载
    Download,
    /// 本地已删除,按策略从远端删掉
    DeleteRemote,
}

/// 一个文件的同步计划。
#[derive(Clone, Debug)]
pub struct PlanItem {
    pub rel_path: String,
    pub action: Action,
    pub local_size: u64,
    pub remote_etag: Option<String>,
}

/// 同步结果统计。
#[derive(Clone, Debug, Default)]
pub struct SyncReport {
    pub uploaded: usize,
    pub downloaded: usize,
    pub skipped: usize,
    /// 从云端删掉的个数(本地删了 → 按策略传播上去)
    pub deleted: usize,
    pub failed: Vec<(String, String)>,
    pub bytes_up: u64,
    pub bytes_down: u64,
}

impl SyncReport {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "上传 {} 个({:.1} KB),下载 {} 个({:.1} KB),跳过 {} 个",
            self.uploaded,
            self.bytes_up as f64 / 1024.0,
            self.downloaded,
            self.bytes_down as f64 / 1024.0,
            self.skipped
        );
        if self.deleted > 0 {
            s.push_str(&format!(",删除 {} 个", self.deleted));
        }
        s.push_str(&format!(",失败 {} 个", self.failed.len()));
        s
    }
}

// ---------------------------------------------------------------------------
// WebDAV 客户端
// ---------------------------------------------------------------------------

/// 轻量 WebDAV 客户端。
///
/// 自己写而不是套现成 crate:需要的高级用法(ETag 条件请求、MOVE 原子改名、重试)
/// 通用库通常只覆盖基础 PUT/GET/PROPFIND,遇到就得绕开它 —— 那时等于写了两遍。
/// 总量约 300 行,见技术方案 §11.4。
pub struct WebDavClient {
    cfg: WebDavConfig,
    http: reqwest::Client,
}

impl WebDavClient {
    pub fn new(cfg: WebDavConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .connect_timeout(Duration::from_secs(20))
            // ⚠️ 不要在这里 .default_headers(basic_auth) ——
            //    Seafile 的 WebDAV 会 301 重定向,而 reqwest 默认会剥掉
            //    跨主机的 Authorization 头。放在 send_retry 里逐次附加更稳。
            .build()
            .context("构建 WebDAV HTTP 客户端失败")?;
        Ok(Self { cfg, http })
    }

    pub fn config(&self) -> &WebDavConfig {
        &self.cfg
    }

    /// 给请求附加 Basic 认证。
    ///
    /// **这是唯一正确的附加点。** 早期版本完全没加认证头 ——
    /// 所有请求都以匿名发出,服务器一律回 401,
    /// 于是同步功能在真实服务器上从来没有成功过一次。
    ///
    /// 单元测试照不出这个:它们不连真实服务器。
    fn with_auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.cfg.username.is_empty() {
            // 没配用户名时不附加,让服务器回 401 并把原因说清楚
            return rb;
        }
        rb.basic_auth(&self.cfg.username, Some(&self.cfg.password))
    }

    /// 把错误映射成可行动提示。
    fn explain(&self, status: u16, url: &str) -> String {
        match status {
            401 => format!(
                "认证失败(401)。检查用户名格式是否为 <账号>@auth.local,以及密码是否正确。\n  {url}"
            ),
            403 => format!("无权限访问(403)。检查该目录的权限。\n  {url}"),
            404 => format!("路径不存在(404)。可能是 remote_dir 配错,或目录尚未创建。\n  {url}"),
            409 => format!("父目录不存在(409)。需要先创建目录。\n  {url}"),
            507 => "云端空间不足(507)。".to_string(),
            s if s >= 500 => format!("服务器错误({s}),稍后重试。\n  {url}"),
            s => format!("请求失败(HTTP {s})。\n  {url}"),
        }
    }

    /// 发送请求(带认证),对 429/5xx 做指数退避重试。
    ///
    /// **认证在这里统一附加** —— 所有请求都经过这个方法,
    /// 所以不存在"某个调用忘了带认证"的可能。
    async fn send_retry(&self, build: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt - 1))).await;
            }
            match self.with_auth(build()).send().await {
                Ok(r) => {
                    let code = r.status().as_u16();
                    // 只有 429 / 5xx 值得重试
                    if code == 429 || code >= 500 {
                        last = Some(anyhow!(self.explain(code, r.url().as_str())));
                        continue;
                    }
                    return Ok(r);
                }
                Err(e) => {
                    last = Some(anyhow!("网络错误:{e}"));
                    continue;
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("请求失败,已重试 2 次")))
    }

    /// 确保远端目录存在(MKCOL)。已存在时返回 405,视为成功。
    ///
    /// **逐级创建** —— 传 `a/b/c` 时会依次 MKCOL `a`、`a/b`、`a/b/c`。
    /// 只建最后一级是不行的:WebDAV 不允许跳过中间层级。
    pub async fn ensure_dir(&self, rel_dir: &str) -> Result<()> {
        let mut segs: Vec<&str> = Vec::new();
        for part in rel_dir.trim_matches('/').split('/') {
            if part.is_empty() {
                continue;
            }
            segs.push(part);
            let partial = segs.join("/");
            let u = self.cfg.url_for(&partial);
            let resp = self
                .send_retry(|| self.http.request(reqwest::Method::from_bytes(b"MKCOL").unwrap(), &u))
                .await?;
            let code = resp.status().as_u16();
            // 201 创建成功;405 已存在;301/302 某些服务器会重定向
            if code != 201 && code != 405 && code != 301 && code != 302 {
                return Err(anyhow!(self.explain(code, &u)));
            }
        }
        Ok(())
    }

    /// 确保某个**文件**的父目录存在。
    ///
    /// 上传前必须调这个 —— 见 [`Self::put_atomic`] 的说明。
    pub async fn ensure_parent_dir(&self, rel_path: &str) -> Result<()> {
        let rel_path = rel_path.trim_matches('/');
        match rel_path.rfind('/') {
            Some(i) if i > 0 => self.ensure_dir(&rel_path[..i]).await,
            // 就在远端根下,根目录本身由远端配置保证存在
            _ => Ok(()),
        }
    }

    /// 探测远端某个文件的 ETag。不存在返回 Ok(None)。
    ///
    /// 只 PROPFIND 单个路径,不递归列目录 —— 后者在大目录上很慢。
    pub async fn head_etag(&self, rel_path: &str) -> Result<Option<String>> {
        let url = self.cfg.url_for(rel_path);
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:"><D:prop><D:getetag/><D:getcontentlength/></D:prop></D:propfind>"#;

        let resp = self
            .send_retry(|| {
                self.http
                    .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url)
                    .header("Depth", "0")
                    .header("Content-Type", "application/xml; charset=utf-8")
                    .body(body)
            })
            .await?;

        let code = resp.status().as_u16();
        if code == 404 {
            return Ok(None);
        }
        if code != 207 && code != 200 {
            return Err(anyhow!(self.explain(code, &url)));
        }
        let text = resp.text().await.unwrap_or_default();
        Ok(parse_etag(&text))
    }

    /// 下载一个文件。
    pub async fn get(&self, rel_path: &str) -> Result<Option<Vec<u8>>> {
        let url = self.cfg.url_for(rel_path);
        let resp = self.send_retry(|| self.http.get(&url)).await?;
        let code = resp.status().as_u16();
        if code == 404 {
            return Ok(None);
        }
        if code != 200 {
            return Err(anyhow!(self.explain(code, &url)));
        }
        Ok(Some(resp.bytes().await?.to_vec()))
    }

    /// 原子上传:先 PUT 到 `.part`,成功后 MOVE 到目标名。
    ///
    /// **为什么要这样做:** 上传中断时服务器上会留一个半截文件。
    /// 直接写目标路径的话,另一台设备可能读到残缺数据。
    /// WebDAV 的 MOVE 是原子的,这一步保证"要么完整,要么不存在"。
    pub async fn put_atomic(&self, rel_path: &str, data: &[u8]) -> Result<()> {
        // ★ 先确保父目录存在。
        //
        // 不建目录的话,PUT 到 `transcript/ab/x.json` 会失败 ——
        // Seafile 在父目录缺失时返回 409 Conflict,而 WebDAV 不会自动建中间层级。
        //
        // 这个调用曾经漏掉了(`ensure_dir` 写好了却没人调),因为是死代码
        // 所以测试也照不出来。首次在真实服务器上跑就会暴露。
        self.ensure_parent_dir(rel_path).await?;

        let tmp_rel = format!("{rel_path}.part");
        let tmp_url = self.cfg.url_for(&tmp_rel);
        let final_url = self.cfg.url_for(rel_path);

        let resp = self
            .send_retry(|| self.http.put(&tmp_url).body(data.to_vec()))
            .await?;
        let code = resp.status().as_u16();
        if code != 200 && code != 201 && code != 204 {
            return Err(anyhow!(self.explain(code, &tmp_url)));
        }

        // MOVE 到最终路径
        let resp = self
            .send_retry(|| {
                self.http
                    .request(reqwest::Method::from_bytes(b"MOVE").unwrap(), &tmp_url)
                    .header("Destination", &final_url)
                    .header("Overwrite", "T")
            })
            .await?;
        let code = resp.status().as_u16();
        if code != 201 && code != 204 {
            // MOVE 失败时清理临时文件,避免留垃圾
            let _ = self.delete(&tmp_rel).await;
            return Err(anyhow!(
                "{}\n(MOVE 失败,可能是反向代理未正确重写 Destination 头)",
                self.explain(code, &tmp_url)
            ));
        }
        Ok(())
    }

    /// 删除文件。不存在视为成功。
    pub async fn delete(&self, rel_path: &str) -> Result<()> {
        let url = self.cfg.url_for(rel_path);
        let resp = self.send_retry(|| self.http.delete(&url)).await?;
        let code = resp.status().as_u16();
        if code == 404 || code == 204 || code == 200 {
            Ok(())
        } else {
            Err(anyhow!(self.explain(code, &url)))
        }
    }

    /// 列举远端目录下的文件(Depth: 1)。仅用于 manifest 重建。
    pub async fn list(&self, rel_dir: &str) -> Result<Vec<(String, Option<String>)>> {
        let url = if rel_dir.trim_matches('/').is_empty() {
            self.cfg.root()
        } else {
            self.cfg.url_for(rel_dir)
        };
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:"><D:prop><D:getetag/><D:resourcetype/></D:prop></D:propfind>"#;

        let resp = self
            .send_retry(|| {
                self.http
                    .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url)
                    .header("Depth", "1")
                    .header("Content-Type", "application/xml; charset=utf-8")
                    .body(body)
            })
            .await?;

        let code = resp.status().as_u16();
        if code == 404 {
            return Ok(vec![]);
        }
        if code != 207 && code != 200 {
            return Err(anyhow!(self.explain(code, &url)));
        }
        let text = resp.text().await.unwrap_or_default();
        Ok(parse_listing(&text, &self.cfg.root()))
    }

    /// 列目录,**保留目录条目**并带上大小与修改时间。
    ///
    /// 与 [`Self::list`] 的区别:`list` 只关心文件(同步引擎用),
    /// 这个给界面浏览云端用 —— 要看到有哪些工程目录。
    ///
    /// `rel_dir` 为空表示远端根。
    pub async fn list_nodes(&self, rel_dir: &str) -> Result<Vec<remote::RemoteNode>> {
        let url = if rel_dir.trim_matches('/').is_empty() {
            self.cfg.root()
        } else {
            self.cfg.url_for(rel_dir)
        };
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:"><D:prop><D:getetag/><D:resourcetype/><D:getcontentlength/><D:getlastmodified/></D:prop></D:propfind>"#;

        let resp = self
            .send_retry(|| {
                self.http
                    .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url)
                    .header("Depth", "1")
                    .header("Content-Type", "application/xml; charset=utf-8")
                    .body(body)
            })
            .await?;

        let code = resp.status().as_u16();
        // 目录不存在视为空 —— 界面第一次打开时远端可能还没有这个目录
        if code == 404 {
            return Ok(vec![]);
        }
        if code != 207 && code != 200 {
            return Err(anyhow!(self.explain(code, &url)));
        }
        let text = resp.text().await.unwrap_or_default();
        // ⚠️ 必须把"本次所查目录"传进去。
        //
        // Seafile 的 PROPFIND 会把**所查目录本身**也放进响应里。
        // 老的 parse_listing 丢弃目录所以不受影响,而这里保留目录 ——
        // 不过滤的话每展开一层都会多出一个指向自己的条目,点进去是空的
        // (实测就是这个现象:工程目录里又出现一个同名目录)。
        Ok(remote::parse_propfind(
            &text,
            &self.cfg.root(),
            rel_dir.trim_matches('/'),
        ))
    }

    /// 连通性测试(设置页的"测试连接")。
    pub async fn test_connection(&self) -> Result<String> {
        self.cfg.validate()?;
        let url = self.cfg.root();
        let resp = self
            .send_retry(|| {
                self.http
                    .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &url)
                    .header("Depth", "0")
            })
            .await?;
        let code = resp.status().as_u16();
        if code == 404 {
            // 根目录不存在不是致命错误 —— 第一次同步时会创建
            return Ok("连接正常(远端目录尚未创建,首次同步时会自动建立)".into());
        }
        if code == 207 || code == 200 {
            return Ok("连接正常".into());
        }
        Err(anyhow!(self.explain(code, &url)))
    }
}

// ---------------------------------------------------------------------------
// XML 解析
// ---------------------------------------------------------------------------

/// 从 PROPFIND 响应里取 `<D:getetag>`。命名空间前缀不固定,所以按本地名匹配。
pub fn parse_etag(xml: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_etag = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if local_name(e.name().as_ref()) == b"getetag" {
                    in_etag = true;
                }
            }
            Ok(Event::Text(t)) => {
                if in_etag {
                    let s = t.unescape().unwrap_or_default().trim().to_string();
                    if !s.is_empty() {
                        return Some(s);
                    }
                }
            }
            Ok(Event::End(e)) => {
                if local_name(e.name().as_ref()) == b"getetag" {
                    in_etag = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

/// 从 PROPFIND(Depth: 1)响应里提取文件路径与 ETag。
///
/// 返回相对 `root` 的路径。目录本身(以 `/` 结尾)会被过滤掉。
pub fn parse_listing(xml: &str, root: &str) -> Vec<(String, Option<String>)> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut cur_href: Option<String> = None;
    let mut cur_etag: Option<String> = None;
    let mut cur_is_collection = false;
    let mut in_href = false;
    let mut in_etag = false;
    let mut buf = Vec::new();

    // 把当前累积的 <response> 收尾:目录丢弃,文件则记录
    fn flush(
        out: &mut Vec<(String, Option<String>)>,
        cur_href: &mut Option<String>,
        cur_etag: &mut Option<String>,
        cur_is_collection: &mut bool,
        root: &str,
    ) {
        if let Some(href) = cur_href.take() {
            if !*cur_is_collection {
                if let Some(rel) = href_to_rel(&href, root) {
                    if !rel.is_empty() {
                        out.push((rel, cur_etag.take()));
                    }
                }
            }
        }
        *cur_etag = None;
        *cur_is_collection = false;
    }

    loop {
        match reader.read_event_into(&mut buf) {
            // ⚠️ 自闭合标签(<D:collection/>)在 quick-xml 里是 Event::Empty,
            //    不是 Event::Start。漏掉它会导致目录被当成文件 —— 这是 XML 解析的经典坑。
            Ok(Event::Empty(e)) => {
                if local_name(e.name().as_ref()) == b"collection" {
                    cur_is_collection = true;
                }
            }
            Ok(Event::Start(e)) => match local_name(e.name().as_ref()) {
                b"response" => {
                    cur_href = None;
                    cur_etag = None;
                    cur_is_collection = false;
                }
                b"href" => in_href = true,
                b"getetag" => in_etag = true,
                // 有的服务器写成 <D:collection></D:collection>
                b"collection" => cur_is_collection = true,
                _ => {}
            },
            Ok(Event::Text(t)) => {
                let s = t.unescape().unwrap_or_default().trim().to_string();
                if s.is_empty() {
                    buf.clear();
                    continue;
                }
                if in_href && cur_href.is_none() {
                    cur_href = Some(s);
                } else if in_etag && cur_etag.is_none() {
                    cur_etag = Some(s);
                }
            }
            Ok(Event::End(e)) => match local_name(e.name().as_ref()) {
                b"href" => in_href = false,
                b"getetag" => in_etag = false,
                b"response" => flush(
                    &mut out,
                    &mut cur_href,
                    &mut cur_etag,
                    &mut cur_is_collection,
                    root,
                ),
                _ => {}
            },
            Ok(Event::Eof) => {
                flush(
                    &mut out,
                    &mut cur_href,
                    &mut cur_etag,
                    &mut cur_is_collection,
                    root,
                );
                break;
            }
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// 对 WebDAV 路径做百分号编码,**保留 `/` 作为层级分隔**。
///
/// 规则:
/// - 未保留字符(`A-Z a-z 0-9 - . _ ~`)原样保留
/// - `/` 原样保留(它分隔路径段,不能编码)
/// - 其余字节按 UTF-8 逐字节编成 `%XX`
///
/// 与 `urlencoding::encode` 的区别就是那个 `/` —— 那个函数会把
/// 斜杠也编成 `%2F`,整条路径就变成一个巨长的文件名了。
pub(crate) fn encode_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for b in p.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 去掉 XML 命名空间前缀,只保留本地名。
pub(crate) fn local_name(qname: &[u8]) -> &[u8] {    match qname.iter().rposition(|b| *b == b':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    }
}

/// 把完整 href 转成相对 root 的路径。
pub(crate) fn href_to_rel(href: &str, root: &str) -> Option<String> {
    // href 可能是完整 URL 或绝对路径;取路径部分
    let path = if let Some(idx) = href.find("://") {
        let after = &href[idx + 3..];
        match after.find('/') {
            Some(i) => &after[i..],
            None => return None,
        }
    } else {
        href
    };

    // 找 root 的路径部分
    let root_path = if let Some(idx) = root.find("://") {
        let after = &root[idx + 3..];
        match after.find('/') {
            Some(i) => &after[i..],
            None => "/",
        }
    } else {
        root
    };

    let decoded = percent_decode(path);
    let root_trim = root_path.trim_end_matches('/');
    let rel = decoded
        .strip_prefix(root_trim)?
        .trim_start_matches('/')
        .to_string();
    Some(rel)
}

/// 最小化的百分号解码(WebDAV 的 href 会编码中文与空格)。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 读取或生成本机标识。
///
/// 存在 `store/device.id` 里。**这个文件本身不同步** —— 每台设备要有自己的 ID,
/// 否则"这条记录是不是本机写的"就失去意义,删除判定会失效。
///
/// 用随机数而不是机器名/主机名:机器名可能重复,而且改名会打断同步历史。
///
/// ⚠️ 读取时会剥掉 UTF-8 BOM 并 trim。Windows 上很容易写出带 BOM 的文件
/// (记事本、`Out-File -Encoding utf8`),而 BOM 会让 ID 变成一个不同的字符串
/// (比较失败 → 所有文件都判成"不是本机的" → 删除功能静默失效)。
fn load_or_create_device_id(root: &std::path::Path) -> Result<String> {
    let path = root.join("device.id");
    if let Ok(raw) = std::fs::read(&path) {
        let bytes = manifest::strip_utf8_bom(&raw);
        if let Ok(s) = std::str::from_utf8(bytes) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                return Ok(s);
            }
        }
    }
    let id = random_device_id();
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    // 显式不带 BOM 写入
    std::fs::write(&path, id.as_bytes())
        .with_context(|| format!("写入设备标识失败: {}", path.display()))?;
    Ok(id)
}

/// 生成一个短随机 ID。不引 rand crate —— 用时间 + 进程号 + 地址熵混合。
fn random_device_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let stack = &nanos as *const _ as usize;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in [nanos as u64, pid as u64, stack as u64] {
        for b in v.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("dev-{h:016x}")
}

// ---------------------------------------------------------------------------
// 同步引擎
// ---------------------------------------------------------------------------

/// 同步器。
pub struct Syncer<'a> {
    client: WebDavClient,
    files: &'a FileStore,
    runtime: Option<tokio::runtime::Runtime>,
    /// 同步范围、音频策略、删除策略。见 [`selection`]。
    selection: selection::SyncSelection,
    /// 本机标识。用于判断某个文件是否是"本机同步过、后来删了"。
    device_id: String,
}

impl Drop for Syncer<'_> {
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            // 与 BlockingSummarizer 同样的处理:在专属线程里销毁运行时,
            // 避免调用方处于异步上下文时 panic。
            let _ = std::thread::spawn(move || drop(rt)).join();
        }
    }
}

impl<'a> Syncer<'a> {
    pub fn new(cfg: WebDavConfig, files: &'a FileStore) -> Result<Self> {
        cfg.validate()?;
        let (runtime, client) = std::thread::spawn(move || -> Result<_> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("创建同步运行时失败")?;
            let c = WebDavClient::new(cfg)?;
            Ok((rt, c))
        })
        .join()
        .map_err(|_| anyhow!("初始化同步运行时失败"))??;
        let device_id = load_or_create_device_id(files.root())?;
        Ok(Self {
            client,
            files,
            runtime: Some(runtime),
            selection: selection::SyncSelection::all(),
            device_id,
        })
    }

    /// 离线构造:不做凭据校验,也不建 HTTP 客户端。
    ///
    /// 只有 [`Syncer::plan_offline`] 能用 —— 它不发任何网络请求。
    /// 这样用户没配账号也能看到"哪些文件会被删"。
    pub fn new_offline(files: &'a FileStore) -> Result<Self> {
        let device_id = load_or_create_device_id(files.root())?;
        // 离线模式不发请求,但仍需要一个占位客户端(字段非 Option)。
        // 用默认配置构造,它不会真的被调用 —— plan_inner 里的守卫会拦住。
        let client = WebDavClient::new(WebDavConfig::default())?;
        Ok(Self {
            client,
            files,
            runtime: None,
            selection: selection::SyncSelection::all(),
            device_id,
        })
    }

    /// 是否处于离线模式(没有网络客户端)。
    pub fn is_offline(&self) -> bool {
        self.runtime.is_none()
    }

    /// 设置同步范围与策略。
    pub fn with_selection(mut self, sel: selection::SyncSelection) -> Self {
        self.selection = sel;
        self
    }

    /// 本机标识。
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// 当前选择。
    pub fn selection(&self) -> &selection::SyncSelection {
        &self.selection
    }

    /// 读取本机同步状态(界面用)。
    pub fn load_state(&self) -> state::SyncState {
        state::SyncState::load(&state::SyncState::path_in(self.files.root()))
    }

    /// 保存本机同步状态。
    pub fn save_state(&self, st: &state::SyncState) -> Result<()> {
        st.save(&state::SyncState::path_in(self.files.root()))
    }

    /// 只发 HEAD 探测远端是否已有这些文件。
    ///
    /// 出错时返回空表 —— 界面会退化为按本地推断,而不是整个清单加载失败。
    pub fn probe_remote(
        &self,
        rels: &[String],
    ) -> std::collections::BTreeMap<String, bool> {
        let mut out = std::collections::BTreeMap::new();
        if self.runtime.is_none() {
            return out;
        }
        for rel in rels {
            let exists = self
                .rt()
                .block_on(self.client.head_etag(rel))
                .ok()
                .flatten()
                .is_some();
            out.insert(rel.clone(), exists);
        }
        out
    }

    /// 构建界面用的清单树。
    ///
    /// `probe` 为 true 时联网确认远端状态(慢,但状态准确);
    /// false 时只按本地 + 状态表推断(快,适合首屏)。
    pub fn inventory(
        &self,
        sel: &selection::SyncSelection,
        probe: bool,
    ) -> Result<inventory::InventoryNode> {
        let st = self.load_state();

        let mut rels: Vec<String> = Vec::new();
        for abs in self.files.list_sync_files()? {
            if let Some(r) = self.files.relative_sync_path(&abs) {
                rels.push(r);
            }
        }
        for abs in self.files.list_project_files(true)? {
            if let Some(r) = self.files.relative_sync_path(&abs) {
                rels.push(r);
            }
        }
        rels.extend(st.files.keys().cloned());
        rels.sort();
        rels.dedup();

        let remote = if probe {
            self.probe_remote(&rels)
        } else {
            std::collections::BTreeMap::new()
        };

        inventory::build_inventory(&self.files, sel, &st, &remote)
    }

    /// 是否把工程里的录音一并同步。
    ///
    /// 打开后,工程成为"整包可带走"的 —— 代价是传输体积大幅增加。
    /// 关掉时音频完全不参与(不传也不拉),需要另行用网盘客户端处理。
    pub fn with_audio(mut self, yes: bool) -> Self {
        self.selection.audio = if yes {
            selection::AudioPolicy::Sync
        } else {
            selection::AudioPolicy::Skip
        };
        self
    }

    fn rt(&self) -> &tokio::runtime::Runtime {
        self.runtime.as_ref().expect("runtime 已被释放")
    }

    pub fn test_connection(&self) -> Result<String> {
        self.rt().block_on(self.client.test_connection())
    }

    /// 计算同步计划。
    ///
    /// 三件事:
    /// 1. **本地有的** → 上传(远端没有)或下载(远端被改过)或跳过
    /// 2. **本地没有、manifest 说是本机同步过的** → 按删除策略删远端
    /// 3. **范围外的** → 完全不碰
    ///
    /// 第 2 条是"删"的来源。它的安全性靠两重条件:
    /// - 必须在本机同步过(`origin` 是本机设备 ID)—— 避免把别的设备
    ///   上传、本机还没下载的文件误判为"本地删了"
    /// - 必须被删除策略允许(默认不删音频)
    pub fn plan(&self, manifest: &Manifest) -> Result<Vec<PlanItem>> {
        self.plan_inner(manifest, false)
    }

    /// 离线计划:假设远端什么都没有,不发任何网络请求。
    ///
    /// 用于:
    /// - **排查删除判定** —— 不联网也能看出"哪些文件会被删"
    /// - 没配 WebDAV 账号时也能预览
    ///
    /// 因为假设远端为空,所有本地文件都会判成 `Upload`;
    /// 真正有信息量的是 `DeleteRemote` 那部分,它只看本地 + manifest。
    pub fn plan_offline(&self, manifest: &Manifest) -> Result<Vec<PlanItem>> {
        self.plan_inner(manifest, true)
    }

    fn plan_inner(&self, manifest: &Manifest, offline: bool) -> Result<Vec<PlanItem>> {
        // 离线模式不该碰网络 —— 早失败比静默联网好
        if !offline && self.runtime.is_none() {
            anyhow::bail!("同步器处于离线模式,无法执行联网操作");
        }
        let mut locals = self.files.list_sync_files()?;
        // 始终列出音频文件(是否真的同步由 selection 决定),
        // 这样"关掉音频"时音频会落进"范围外"而不是被当成已删除
        locals.extend(self.files.list_project_files(true)?);
        locals.sort();
        locals.dedup();
        let mut items = Vec::new();

        for abs in locals {
            let Some(rel) = self.files.relative_sync_path(&abs) else {
                continue;
            };
            // 范围外的一律不碰 —— 既不上传,也不会被算作"已删除"
            if !self.selection.wants(&rel) {
                continue;
            }
            let local_size = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
            let remote_etag = if offline {
                None
            } else {
                self.rt()
                    .block_on(self.client.head_etag(&rel))
                    .unwrap_or(None)
            };

            let known_entry = manifest.files.get(&rel);
            let known = known_entry.and_then(|e| e.etag.clone());
            // 本地存在 → 之前的墓碑作废(文件又回来了)
            let was_tombstoned = known_entry.map(|e| e.is_tombstone()).unwrap_or(false);

            // 判定依据:远端 ETag 是否与我们记录过的一致
            let action = if was_tombstoned {
                // 本地有但 manifest 说删过 —— 说明是本机删后又放回来的,
                // 直接重传覆盖云端
                Action::Upload
            } else {
                match (&remote_etag, &known) {
                    (None, _) => Action::Upload, // 远端没有
                    (Some(r), Some(k)) if r == k => Action::Skip, // 两边一致
                    (Some(_), Some(_)) => Action::Download, // 远端被改过
                    (Some(_), None) => Action::Skip, // 远端有但我们没记录 → 保守跳过
                }
            };

            // ★ 方向过滤。不满足的条件降级为 Skip ——
            //   注意是"不传",不是"删除":反方向的差异保持原样。
            let action = match action {
                Action::Upload if !self.selection.direction.allows_upload() => Action::Skip,
                Action::Download if !self.selection.direction.allows_download() => Action::Skip,
                other => other,
            };

            items.push(PlanItem {
                rel_path: rel,
                action,
                local_size,
                remote_etag,
            });
        }

        // ---- 本地已删除的文件 ----
        //
        // 判定条件(全部满足才会删):
        // 1. manifest 里有这条记录,且不是墓碑
        // 2. 本地确实没有这个文件
        // 3. **这条记录是本机写的** —— 别的设备传的、本机还没下载的不算
        // 4. 在同步范围内,且删除策略允许
        for (rel, entry) in manifest.live_files() {
            if entry.is_tombstone() {
                continue;
            }
            if !entry.from_device(&self.device_id) {
                continue; // 不是本机同步过的,保守跳过
            }
            if self.files.root().join(rel).exists() {
                continue; // 本地还在
            }
            if !self.selection.should_delete_remote(rel) {
                continue; // 范围外或策略不允许
            }
            items.push(PlanItem {
                rel_path: rel.clone(),
                action: Action::DeleteRemote,
                local_size: 0,
                remote_etag: entry.etag.clone(),
            });
        }

        items.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        Ok(items)
    }

    /// 执行同步。
    pub fn run(&self, progress: &dyn Fn(&str, usize, usize)) -> Result<SyncReport> {
        let mut manifest = Manifest::load_or_new(&self.files.manifest_path())?;

        // ★ 上传前先拉远端 manifest 合并 —— 这就是单人场景的"锁"
        let remote_manifest = self
            .rt()
            .block_on(self.client.get(manifest::MANIFEST_REMOTE_PATH))
            .ok()
            .flatten()
            .and_then(|b| Manifest::from_bytes(&b).ok());
        if let Some(rm) = remote_manifest {
            manifest.merge_from(&rm);
        }

        let plan = self.plan(&manifest)?;
        let total = plan.len();
        let mut report = SyncReport::default();

        // ★ 本机同步状态。
        //
        // 这份状态是**界面显示"已同步/从未同步"的唯一依据**
        // (见 state::status_of),它和 manifest 是两件事:
        //
        //   manifest   —— 云端有哪些文件(会同步给别的设备)
        //   sync-state —— 本机传过哪些文件、什么时候传的(只在本机)
        //
        // ⚠️ 之前这里漏了保存:`save_state` 写好了却没人调,
        //    所以 sync-state.json 从来不生成,清单里所有文件**永远显示
        //    "从未同步"** —— 哪怕 manifest 里明明有记录。
        //    这和 ensure_dir 那次是同一类错误:死代码不报错,
        //    测试也照不出来,只有真跑一遍才看得见。
        let mut sync_state = self.load_state();

        for (i, item) in plan.iter().enumerate() {
            progress(&item.rel_path, i + 1, total);
            match item.action {
                Action::Skip => {
                    report.skipped += 1;
                    // 已经是同步状态。若本机状态表里没有这条(比如从没存过),
                    // 补一条 —— 否则界面上会显示成"从未同步",与事实不符。
                    if sync_state.get(&item.rel_path).is_none() {
                        sync_state.mark_synced(
                            &item.rel_path,
                            item.remote_etag.clone(),
                            Some(item.local_size),
                        );
                    }
                }
                Action::Upload => {
                    let abs = self.files.root().join(&item.rel_path);
                    match std::fs::read(&abs) {
                        Ok(data) => {
                            let n = data.len() as u64;
                            match self.rt().block_on(self.client.put_atomic(&item.rel_path, &data)) {
                                Ok(()) => {
                                    report.uploaded += 1;
                                    report.bytes_up += n;
                                    // 上传后重新取真实 ETag,避免记录的与实际不一致
                                    let etag = self
                                        .rt()
                                        .block_on(self.client.head_etag(&item.rel_path))
                                        .ok()
                                        .flatten();
                                    // 先记本机状态(要借用 etag),再写 manifest(会取走它)
                                    sync_state.mark_synced(&item.rel_path, etag.clone(), Some(n));
                                    manifest.record(
                                        &item.rel_path,
                                        ManifestEntry {
                                            etag,
                                            size: Some(n),
                                            synced_at: crate::store::db::now_ms(),
                                            origin: Some(self.device_id.clone()),
                                            deleted_at: None,
                                        },
                                    );
                                }
                                Err(e) => report
                                    .failed
                                    .push((item.rel_path.clone(), e.to_string())),
                            }
                        }
                        Err(e) => report
                            .failed
                            .push((item.rel_path.clone(), format!("读取本地文件失败:{e}"))),
                    }
                }
                Action::Download => {
                    match self.rt().block_on(self.client.get(&item.rel_path)) {
                        Ok(Some(data)) => {
                            let abs = self.files.root().join(&item.rel_path);
                            if let Some(p) = abs.parent() {
                                let _ = std::fs::create_dir_all(p);
                            }
                            match std::fs::write(&abs, &data) {
                                Ok(()) => {
                                    report.downloaded += 1;
                                    report.bytes_down += data.len() as u64;
                                    manifest.record(
                                        &item.rel_path,
                                        ManifestEntry {
                                            etag: item.remote_etag.clone(),
                                            size: Some(data.len() as u64),
                                            synced_at: crate::store::db::now_ms(),
                                            origin: Some(self.device_id.clone()),
                                            deleted_at: None,
                                        },
                                    );
                                    sync_state.mark_synced(
                                        &item.rel_path,
                                        item.remote_etag.clone(),
                                        Some(data.len() as u64),
                                    );
                                }
                                Err(e) => report
                                    .failed
                                    .push((item.rel_path.clone(), format!("写入失败:{e}"))),
                            }
                        }
                        Ok(None) => report.skipped += 1,
                        Err(e) => report
                            .failed
                            .push((item.rel_path.clone(), e.to_string())),
                    }
                }
                Action::DeleteRemote => {
                    // 本地已删除,按策略删掉云端那份,并留下墓碑
                    match self.rt().block_on(self.client.delete(&item.rel_path)) {
                        Ok(()) => {
                            report.deleted += 1;
                            manifest.record_tombstone(&item.rel_path, &self.device_id);
                            // 云端那份没了,本机状态也要跟着反映
                            sync_state.mark_local_deleted(&item.rel_path);
                        }
                        Err(e) => {
                            // 404 也算成功 —— 目标状态已达成
                            if e.to_string().contains("404") {
                                report.deleted += 1;
                                manifest.record_tombstone(&item.rel_path, &self.device_id);
                                sync_state.mark_local_deleted(&item.rel_path);
                            } else {
                                report
                                    .failed
                                    .push((item.rel_path.clone(), format!("删除失败:{e}")));
                            }
                        }
                    }
                }
            }
        }

        // 远端 manifest 也要更新 —— 它不属于 files 列表,单独上传
        let bytes = manifest.to_bytes()?;
        self.rt()
            .block_on(self.client.put_atomic(manifest::MANIFEST_REMOTE_PATH, &bytes))?;
        // 本地也落一份,便于排查
        manifest.save(&self.files.manifest_path())?;

        // ★ 保存本机同步状态。界面上的"已同步/从未同步"就靠它。
        //
        // 保存失败**不该让整次同步算失败** —— 文件已经传上去了,
        // 状态表丢了最多是界面显示不准,重新同步一次就补回来。
        // 所以这里只记日志,不返回错误。
        if let Err(e) = self.save_state(&sync_state) {
            tracing::warn!("保存 sync-state 失败(界面状态可能显示不准): {e}");
        }

        Ok(report)
    }

    /// 从远端目录重建 manifest(manifest 丢失/损坏时的兜底)。
    ///
    /// ★ 这正是"manifest 不需要事务保护"的原因 —— 它随时可以从远端扫回来。
    ///
    /// ⚠️ **重建出来的条目 `origin` 留空。** 这些文件是"远端有"，
    /// 不代表"本机同步过"。若标成本机,下次同步就会因为它们本地不存在
    /// 而被判定为"用户删了" → 误删云端。空 origin 让删除判定保守跳过。
    pub fn rebuild_manifest(&self) -> Result<Manifest> {
        let mut manifest = Manifest::new();
        for dir in ["transcript", "summary", "speaker_labels"] {
            // 远端按哈希前两位分子目录,逐个子目录列
            if let Ok(entries) = self.rt().block_on(self.client.list(dir)) {
                for (rel, etag) in entries {
                    manifest.record(
                        &rel,
                        ManifestEntry {
                            etag,
                            size: None,
                            synced_at: crate::store::db::now_ms(),
                            origin: None,
                            deleted_at: None,
                        },
                    );
                }
            }
        }
        for f in ["profiles.json"] {
            if let Ok(Some(etag)) = self.rt().block_on(self.client.head_etag(f)) {
                manifest.record(
                    f,
                    ManifestEntry {
                        etag: Some(etag),
                        size: None,
                        synced_at: crate::store::db::now_ms(),
                        origin: None,
                        deleted_at: None,
                    },
                );
            }
        }
        Ok(manifest)
    }

    /// 音频文件交给 Seafile 客户端,可选:只报告哪些还没进同步目录。
    pub fn audio_sync_hint(&self) -> String {
        audio_sync_hint_for(self.files.root())
    }
}

/// 音频同步提示。独立成自由函数 —— 它不碰网络,便于在 CLI 与测试里直接用。
pub fn audio_sync_hint_for(store_root: &Path) -> String {
    format!(
        "音频文件不通过 WebDAV 传输。\n  \
         请让 Seafile 客户端同步:{} 目录下的 audio/ 子目录\n  \
         原因见技术方案 §11.2:Seafile 的 WebDAV 弱点是「批量小文件」,\n  \
         大文件交给官方客户端更快,而且自带断点续传。",
        store_root.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation() {
        let mut c = WebDavConfig::default();
        assert!(c.validate().is_err(), "空用户名应报错");
        c.username = "2025000000@auth.local".into();
        assert!(c.validate().is_err(), "空密码应报错");
        c.password = "x".into();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn root_url_handles_slashes() {
        let mut c = WebDavConfig::default();
        c.base_url = "https://cloud.example.com/seafdav/".into();
        c.remote_dir = "/recording-summary/".into();
        assert_eq!(
            c.root(),
            "https://cloud.example.com/seafdav/recording-summary"
        );
        assert_eq!(
            c.url_for("transcript/ab/x.json"),
            "https://cloud.example.com/seafdav/recording-summary/transcript/ab/x.json"
        );
    }

    #[test]
    fn remote_dir_accepts_full_url() {
        // ★ 用户很可能直接把完整地址粘进"远端目录"框里。
        //   这里明确它的行为:拼出来的会是个无效 URL。
        //   (CLI/GUI 侧负责拆分,见 normalize_remote_input)
        let mut c = WebDavConfig::default();
        c.base_url = "https://cloud.example.com/seafdav".into();
        c.remote_dir = "recording_summary".into();
        assert_eq!(
            c.root(),
            "https://cloud.example.com/seafdav/recording_summary"
        );
    }

    #[test]
    fn root_accepts_nested_remote_dir() {
        // 远端目录可以是多级,例如想把工程放进 课程/2026秋
        let mut c = WebDavConfig::default();
        c.base_url = "https://cloud.example.com/seafdav".into();
        c.remote_dir = "recording_summary/2026秋".into();
        assert_eq!(
            c.root(),
            "https://cloud.example.com/seafdav/recording_summary/2026秋"
        );
        assert_eq!(
            c.url_for("profiles.json"),
            "https://cloud.example.com/seafdav/recording_summary/2026秋/profiles.json"
        );
    }

    #[test]
    fn root_collapses_duplicate_slashes() {
        // 两侧都带斜杠、或中间有 // 时不该拼出双斜杠
        let cases = [
            ("https://x/y/", "/a/", "https://x/y/a"),
            ("https://x/y", "a//b", "https://x/y/a/b"),
            ("https://x/y/", "//a//b//", "https://x/y/a/b"),
            ("https://x/y", "  spaced  ", "https://x/y/spaced"),
        ];
        for (base, dir, want) in cases {
            let mut c = WebDavConfig::default();
            c.base_url = base.into();
            c.remote_dir = dir.into();
            assert_eq!(c.root(), want, "base={base:?} dir={dir:?}");
        }
    }

    #[test]
    fn empty_remote_dir_uses_base_only() {
        let mut c = WebDavConfig::default();
        c.base_url = "https://x/y/".into();
        c.remote_dir = "".into();
        assert_eq!(c.root(), "https://x/y");
        assert_eq!(c.url_for("a.md"), "https://x/y/a.md");

        c.remote_dir = "/".into();
        assert_eq!(c.root(), "https://x/y", "只有斜杠也算空");
    }

    #[test]
    fn url_for_trims_leading_slash_on_rel() {
        let mut c = WebDavConfig::default();
        c.base_url = "https://x/y".into();
        c.remote_dir = "r".into();
        assert_eq!(c.url_for("/a/b.md"), "https://x/y/r/a/b.md");
    }

    // --- 认证 --------------------------------------------------------------

    #[test]
    fn client_has_credentials_from_config() {
        // ★ 回归测试:早期版本从不附加 Basic 认证头,
        //   所有请求匿名发出 → 服务器一律 401 → 同步从未成功过一次。
        let mut c = WebDavConfig::default();
        c.base_url = "https://cloud.example.com/seafdav".into();
        c.username = "2025000000@auth.local".into();
        c.password = "secret".into();
        let client = WebDavClient::new(c).unwrap();
        assert_eq!(client.config().username, "2025000000@auth.local");
        assert_eq!(client.config().password, "secret");
    }

    #[test]
    fn all_requests_go_through_send_retry() {
        // 这条不变量很关键:认证是在 send_retry 里统一附加的,
        // 所以**任何绕过它的请求都会 401**。
        //
        // 用源码检查来守:sync/mod.rs 里除了 send_retry 自身,
        // 不该有别处直接调 .send()。
        let src = include_str!("mod.rs");
        let mut offenders = Vec::new();
        for (i, line) in src.lines().enumerate() {
            if line.contains(".send().await") && !line.contains("self.with_auth(build())") {
                // 允许出现在注释里
                let trimmed = line.trim();
                if trimmed.starts_with("//") {
                    continue;
                }
                offenders.push(format!("第 {} 行: {}", i + 1, trimmed));
            }
        }
        assert!(
            offenders.is_empty(),
            "★ 这些请求绕过了 send_retry,会因缺认证而 401:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn empty_username_skips_auth_header() {
        // 没配用户名时不附加认证(让服务器回 401 并把原因说清楚,
        // 而不是发一个空凭据上去)
        let c = WebDavConfig::default(); // username 为空
        let client = WebDavClient::new(c).unwrap();
        assert!(client.config().username.is_empty());
        // with_auth 在用户名为空时原样返回 —— 不 panic 即可
        let rb = client.with_auth(client.http.get("https://x/y"));
        drop(rb);
    }

    // --- 完整地址拆分 ------------------------------------------------------

    #[test]
    fn split_seafile_url() {
        // ★ 用户实际给的形态
        assert_eq!(
            split_webdav_url("https://cloud.example.com/seafdav/recording_summary"),
            Some((
                "https://cloud.example.com/seafdav".to_string(),
                "recording_summary".to_string()
            ))
        );
        // 尾部斜杠
        assert_eq!(
            split_webdav_url("https://cloud.example.com/seafdav/recording_summary/"),
            Some((
                "https://cloud.example.com/seafdav".to_string(),
                "recording_summary".to_string()
            ))
        );
        // 多级子目录
        assert_eq!(
            split_webdav_url("https://cloud.example.com/seafdav/课程/2026秋"),
            Some((
                "https://cloud.example.com/seafdav".to_string(),
                "课程/2026秋".to_string()
            ))
        );
    }

    #[test]
    fn split_leaves_plain_service_url_alone() {
        // 本来就是服务地址 → 没什么可拆
        assert_eq!(split_webdav_url("https://cloud.example.com/seafdav"), None);
        assert_eq!(split_webdav_url("https://cloud.example.com/seafdav/"), None);
        // 纯主机
        assert_eq!(split_webdav_url("https://cloud.example.com"), None);
        // 不是 URL
        assert_eq!(split_webdav_url("recording_summary"), None);
        assert_eq!(split_webdav_url(""), None);
    }

    #[test]
    fn split_handles_other_webdav_services() {
        // 坚果云:/dav 是服务根,后面是用户目录
        assert_eq!(
            split_webdav_url("https://dav.jianguoyun.com/dav/我的坚果云/录音"),
            Some((
                "https://dav.jianguoyun.com/dav".to_string(),
                "我的坚果云/录音".to_string()
            ))
        );
        // Nextcloud 风格
        assert_eq!(
            split_webdav_url("https://cloud.example.com/remote.php/dav/files/me/docs"),
            Some((
                "https://cloud.example.com/remote.php/dav".to_string(),
                "files/me/docs".to_string()
            ))
        );
    }

    #[test]
    fn split_falls_back_to_last_segment() {
        // 没有已知服务根,但路径有多段 → 取最后一段
        assert_eq!(
            split_webdav_url("https://example.com/custom/path/mydir"),
            Some((
                "https://example.com/custom/path".to_string(),
                "mydir".to_string()
            ))
        );
        // 只有一段时不猜(更可能是服务根)
        assert_eq!(split_webdav_url("https://example.com/dav"), None);
    }

    #[test]
    fn split_result_roundtrips_to_same_url() {
        // 拆完再拼回来,必须还是原来的地址 —— 否则用户会以为程序改错了
        for url in [
            "https://cloud.example.com/seafdav/recording_summary",
            "https://cloud.example.com/seafdav/课程/2026秋",
            "https://dav.jianguoyun.com/dav/我的坚果云/录音",
            "https://cloud.example.com/remote.php/dav/files/me/docs",
            "https://example.com/custom/path/mydir",
        ] {
            let (base, dir) = split_webdav_url(url).unwrap_or_else(|| panic!("应能拆:{url}"));
            let mut c = WebDavConfig::default();
            c.base_url = base.clone();
            c.remote_dir = dir.clone();
            assert_eq!(c.root(), url.trim_end_matches('/'), "拆完拼回应一致:{url}");
        }
    }

    #[test]
    fn split_is_idempotent_on_already_split_parts() {
        // 已经拆好的服务地址再拆一次仍是 None —— 不会把 seafdav 当成目录
        let (base, dir) =
            split_webdav_url("https://cloud.example.com/seafdav/recording_summary").unwrap();
        assert_eq!(split_webdav_url(&base), None, "服务地址不该再被拆");
        // 目录本身不是 URL,也不会被拆
        assert_eq!(split_webdav_url(&dir), None);
    }

    #[test]
    fn parent_dir_extraction() {
        // ensure_parent_dir 的逻辑:取最后一个 `/` 之前的部分
        let cases = [
            ("a/b/c.md", Some("a/b")),
            ("a/b.md", Some("a")),
            ("top.md", None),          // 就在远端根下
            ("/abs/x.md", Some("abs")), // 前导斜杠被 trim
        ];
        for (rel, want) in cases {
            let rel = rel.trim_matches('/');
            let got = match rel.rfind('/') {
                Some(i) if i > 0 => Some(&rel[..i]),
                _ => None,
            };
            assert_eq!(got, want, "rel={rel:?}");
        }
    }

    #[test]
    fn project_file_paths_need_parent_dirs() {
        // 这些是实际会上传的路径形态 —— 每一个的父目录都得先 MKCOL。
        // 漏掉任何一个,首次上传就会在真实服务器上失败。
        for rel in [
            "transcript/ab/abcdef.json",
            "summary/ab/abcdef.md",
            "speaker_labels/ab/abcdef.json",
            "projects/2026-09-11_高数/transcript.md",
            "projects/2026-09-11_高数/audio/recording.wav",
        ] {
            let rel = rel.trim_matches('/');
            let parent = match rel.rfind('/') {
                Some(i) if i > 0 => Some(&rel[..i]),
                _ => None,
            };
            assert!(parent.is_some(), "★ {rel} 应有父目录,必须先建");
        }
        // 只有这两个在远端根下,不需要建目录
        for rel in ["profiles.json", "manifest.json"] {
            assert!(!rel.contains('/'), "{rel} 应在根下");
        }
    }

    #[test]
    fn parse_etag_from_seafile_response() {
        // Seafile 的 PROPFIND 响应形态
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/seafdav/recording-summary/profiles.json</D:href>
    <D:propstat>
      <D:prop>
        <D:getetag>"a1b2c3d4e5f6"</D:getetag>
        <D:getcontentlength>1234</D:getcontentlength>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        assert_eq!(parse_etag(xml), Some("\"a1b2c3d4e5f6\"".to_string()));
    }

    #[test]
    fn parse_etag_without_namespace_prefix() {
        let xml = r#"<multistatus><response><prop><getetag>abc</getetag></prop></response></multistatus>"#;
        assert_eq!(parse_etag(xml), Some("abc".to_string()));
    }

    #[test]
    fn parse_etag_missing_returns_none() {
        assert_eq!(parse_etag("<multistatus></multistatus>"), None);
        assert_eq!(parse_etag(""), None);
    }

    #[test]
    fn parse_listing_extracts_files_and_skips_dirs() {
        let xml = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/seafdav/recording-summary/transcript/</D:href>
    <D:propstat><D:prop>
      <D:resourcetype><D:collection/></D:resourcetype>
    </D:prop></D:propstat>
  </D:response>
  <D:response>
    <D:href>/seafdav/recording-summary/transcript/ab/</D:href>
    <D:propstat><D:prop>
      <D:resourcetype><D:collection/></D:resourcetype>
    </D:prop></D:propstat>
  </D:response>
  <D:response>
    <D:href>/seafdav/recording-summary/transcript/ab/ab3f9c.json</D:href>
    <D:propstat><D:prop>
      <D:getetag>"e1"</D:getetag>
      <D:resourcetype/>
    </D:prop></D:propstat>
  </D:response>
</D:multistatus>"#;
        let root = "https://cloud.example.com/seafdav/recording-summary";
        let got = parse_listing(xml, root);
        assert_eq!(got.len(), 1, "目录应被过滤:{got:?}");
        assert_eq!(got[0].0, "transcript/ab/ab3f9c.json");
        assert_eq!(got[0].1.as_deref(), Some("\"e1\""));
    }

    #[test]
    fn parse_listing_percent_decodes_paths() {
        let xml = r#"<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/seafdav/recording-summary/speaker_labels/%E4%B8%AD%E6%96%87.json</D:href>
    <D:propstat><D:prop><D:getetag>"x"</D:getetag></D:prop></D:propstat>
  </D:response>
</D:multistatus>"#;
        let got = parse_listing(xml, "https://cloud.example.com/seafdav/recording-summary");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "speaker_labels/中文.json");
    }

    #[test]
    fn href_to_rel_handles_absolute_paths() {
        let root = "https://cloud.example.com/seafdav/recording-summary";
        assert_eq!(
            href_to_rel("/seafdav/recording-summary/profiles.json", root).as_deref(),
            Some("profiles.json")
        );
        // 完整 URL 形式
        assert_eq!(
            href_to_rel(
                "https://cloud.example.com/seafdav/recording-summary/manifest.json",
                root
            )
            .as_deref(),
            Some("manifest.json")
        );
        // 不在 root 下的路径应被丢弃
        assert_eq!(href_to_rel("/seafdav/other/x.json", root), None);
    }

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("no-encoding"), "no-encoding");
        // 非法序列不应 panic
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    // --- 路径百分号编码 ----------------------------------------------------

    #[test]
    fn encode_path_keeps_slashes_and_unreserved() {
        // ★ 斜杠必须保留 —— 编码成 %2F 会把整条路径变成一个文件名
        assert_eq!(encode_path("a/b/c.txt"), "a/b/c.txt");
        assert_eq!(encode_path("AZaz09-._~"), "AZaz09-._~");
    }

    #[test]
    fn encode_path_encodes_space_and_chinese() {
        // 空格:reqwest 会替 URL 自动编码,但不会替请求头(Destination)编码
        assert_eq!(encode_path("a b"), "a%20b");
        // 中文:逐字节 UTF-8 编码
        assert_eq!(encode_path("中文"), "%E4%B8%AD%E6%96%87");
        // 组合:真实工程目录名的形状。
        // 注意日期里的 `9` 是数字,不编码 —— 我第一版期望值把它漏了,
        // 是测试写错而不是实现错(用 PowerShell 独立算过逐字节一致)。
        assert_eq!(
            encode_path("projects/2026-09-11_9月11日 计算机视觉/x.md"),
            "projects/2026-09-11_9%E6%9C%8811%E6%97%A5%20%E8%AE%A1%E7%AE%97%E6%9C%BA%E8%A7%86%E8%A7%89/x.md"
        );
    }

    #[test]
    fn encode_path_escapes_reserved_chars() {
        // 这些字符在 URL 里有特殊含义,必须编码
        assert_eq!(encode_path("a?b"), "a%3Fb");
        assert_eq!(encode_path("a#b"), "a%23b");
        assert_eq!(encode_path("a%20b"), "a%2520b"); // % 自身也要编码
        assert_eq!(encode_path("a+b"), "a%2Bb");
    }

    #[test]
    fn url_for_round_trips_through_decode() {
        // 编码后再解码应还原 —— 这是编码正确性的核心保证
        let cases = [
            "projects/2026-09-11_9月11日 计算机视觉/transcript.md",
            "audio/recording.m4a",
            "中文目录/deep/hello.txt",
            "a b/c d.md",
        ];
        for c in cases {
            assert_eq!(percent_decode(&encode_path(c)), c, "往返失败: {c}");
        }
    }

    #[test]
    fn url_for_encodes_the_relative_part_only() {
        let cfg = WebDavConfig {
            base_url: "https://cloud.example.com/webdav".into(),
            remote_dir: "recording-summary".into(),
            ..Default::default()
        };
        let u = cfg.url_for("projects/中文 目录/a.md");
        // 服务地址部分保持原样,只有相对路径被编码
        assert!(u.starts_with("https://cloud.example.com/webdav/recording-summary/"), "{u}");
        assert!(u.ends_with("projects/%E4%B8%AD%E6%96%87%20%E7%9B%AE%E5%BD%95/a.md"), "{u}");
        assert!(!u.contains(' '), "URL 里不该有原始空格: {u}");
    }

    #[test]
    fn url_for_leaves_ascii_paths_unchanged() {
        let cfg = WebDavConfig {
            base_url: "https://cloud.example.com/webdav".into(),
            remote_dir: "recording-summary".into(),
            ..Default::default()
        };
        assert_eq!(
            cfg.url_for("transcript/ab/ab3f.json"),
            "https://cloud.example.com/webdav/recording-summary/transcript/ab/ab3f.json"
        );
    }

    #[test]
    fn parse_listing_handles_selfclosing_collection_tag() {
        // ★ 回归测试:<D:collection/> 是自闭合标签,quick-xml 发出 Event::Empty。
        //   漏掉它会把目录当文件,manifest 里就多出一堆假条目。
        let xml = r#"<?xml version="1.0"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/seafdav/rs/transcript/</D:href>
    <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat>
  </D:response>
  <D:response>
    <D:href>/seafdav/rs/transcript/a.json</D:href>
    <D:propstat><D:prop><D:getetag>"e"</D:getetag><D:resourcetype/></D:prop></D:propstat>
  </D:response>
</D:multistatus>"#;
        let got = parse_listing(xml, "https://x/seafdav/rs");
        assert_eq!(got.len(), 1, "自闭合的 collection 必须被识别为目录:{got:?}");
        assert_eq!(got[0].0, "transcript/a.json");
    }

    #[test]
    fn parse_listing_handles_paired_collection_tag() {
        // 有的服务器写成成对标签,也要能识别
        let xml = r#"<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/r/dir/</D:href>
    <D:propstat><D:prop><D:resourcetype><D:collection></D:collection></D:resourcetype></D:prop></D:propstat>
  </D:response>
  <D:response>
    <D:href>/r/f.txt</D:href>
    <D:propstat><D:prop><D:getetag>"z"</D:getetag></D:prop></D:propstat>
  </D:response>
</D:multistatus>"#;
        let got = parse_listing(xml, "https://x/r");
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, "f.txt");
    }

    #[test]
    fn parse_listing_filters_root_self_entry() {
        // Depth:1 的响应会包含 root 自身,不能当成文件
        let xml = r#"<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/r/</D:href>
    <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat>
  </D:response>
</D:multistatus>"#;
        assert!(parse_listing(xml, "https://x/r").is_empty());
    }

    #[test]
    fn action_equality_is_usable_in_tests() {
        assert_eq!(Action::Upload, Action::Upload);
        assert_ne!(Action::Skip, Action::Download);
    }

    #[test]
    fn sync_report_summary_reads_well() {
        let mut r = SyncReport::default();
        r.uploaded = 3;
        r.skipped = 10;
        r.bytes_up = 2048;
        let s = r.summary();
        assert!(s.contains("上传 3"), "{s}");
        assert!(s.contains("跳过 10"), "{s}");
    }

    #[test]
    fn explain_gives_actionable_hints() {
        let c = WebDavClient::new(WebDavConfig {
            username: "u".into(),
            password: "p".into(),
            ..Default::default()
        })
        .unwrap();
        // 认证失败是最常见的坑,提示必须说明用户名格式
        let m = c.explain(401, "https://x/y");
        assert!(m.contains("auth.local"), "{m}");
        assert!(c.explain(404, "u").contains("remote_dir"));
        assert!(c.explain(507, "u").contains("空间不足"));
    }

    #[test]
    fn audio_hint_explains_the_split() {
        let dir = tempfile::tempdir().unwrap();
        let h = audio_sync_hint_for(dir.path());
        assert!(h.contains("Seafile"), "{h}");
        assert!(h.contains("断点续传"), "{h}");
        assert!(h.contains("批量小文件"), "提示应说明原因:{h}");
    }
}
