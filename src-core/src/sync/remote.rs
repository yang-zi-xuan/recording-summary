//! 云端目录树、单文件传输、以及本地↔云端差异对照。
//!
//! # 为什么单独一个模块
//!
//! [`crate::sync`] 的主体是**同步引擎** —— 它按规则(方向、范围、删除策略)
//! 决定"该做什么",然后整批执行。那是自动化的路径。
//!
//! 这里做的是另一件事:**让用户看见云端、并单独操作某一个文件**。
//! 两者共用 [`WebDavClient`],但决策主体不同 —— 一个是规则,一个是人。
//!
//! 分开还有一个实际好处:用户手动删掉的文件**不写墓碑**。墓碑的语义是
//! "这个文件被删了,请把这个事实同步给别的设备";而手动删除是"我不想要
//! 云端这一份",不该反过来影响其他设备对本地文件的判断。

use anyhow::{Context, Result};
use serde::Serialize;

use super::{href_to_rel, local_name, WebDavClient};

// ---------------------------------------------------------------------------
// 远端节点
// ---------------------------------------------------------------------------

/// 云端的一个条目(文件或目录)。
#[derive(Clone, Debug, Serialize)]
pub struct RemoteNode {
    /// 相对远端根目录的路径(目录不带结尾斜杠)
    pub rel_path: String,
    /// 显示名(路径最后一段)
    pub name: String,
    pub is_dir: bool,
    /// 文件大小(字节)。目录为 None。
    pub size: Option<u64>,
    /// 最后修改时间(RFC 1123 原文,交给前端自己格式化)
    pub modified: Option<String>,
    pub etag: Option<String>,
    /// 子节点。目录才会有;非递归拉取时为空。
    pub children: Vec<RemoteNode>,
}

impl RemoteNode {
    /// 递归统计文件数与总字节数(含自身)。
    pub fn totals(&self) -> (usize, u64) {
        if !self.is_dir {
            return (1, self.size.unwrap_or(0));
        }
        let mut n = 0usize;
        let mut b = 0u64;
        for c in &self.children {
            let (cn, cb) = c.totals();
            n += cn;
            b += cb;
        }
        (n, b)
    }
}

/// WebDAV 的 `getcontentlength` 不一定给,给也可能是目录的 `0`。
fn parse_size(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    t.parse::<u64>().ok()
}

/// 解析 PROPFIND 响应成节点列表。
///
/// 与 [`super::parse_listing`] 的区别:**保留目录**,并额外解析
/// `getcontentlength` 与 `getlastmodified`。
///
/// `root` 是远端根(用来把 href 转成相对路径),
/// `self_rel` 是**本次所查目录**相对 root 的路径 —— 见下面关于自引用的说明。
///
/// # 自引用条目(实测于 Seafile)
///
/// `PROPFIND` 一个目录时,响应里会**同时包含这个目录本身**和它的子项:
///
/// ```text
/// PROPFIND /seafdav/recording-summary/projects
///   → <response> href=.../recording-summary/projects          ← 它自己
///   → <response> href=.../recording-summary/projects/工程A    ← 子项
/// ```
///
/// 老的 [`super::parse_listing`] 丢弃目录,所以这个问题一直没暴露;
/// 这里保留目录,就必须显式过滤掉 `rel_path == self_rel` 的那一条 ——
/// 否则每展开一层都会多出一个"同名子目录",点进去是空的。
///
/// `root` 对应的那一条(空 rel)也会被丢弃。
///
/// 实测注意:自闭合的 `<D:collection/>` 在 quick-xml 里是 `Event::Empty`
/// 而不是 `Event::Start`,两种都要认,否则目录会被当成文件。
pub fn parse_propfind(xml: &str, root: &str, self_rel: &str) -> Vec<RemoteNode> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    /// 一个 `<response>` 里累积的属性。
    ///
    /// 抽成结构体而不是散落的局部变量:每个 response 结束时直接**丢弃整个
    /// 结构体**,不可能出现"漏重置某个字段"——那正是这类解析器最容易出的
    /// bug(is_collection 泄漏会让目录被当成文件,而且只在特定顺序下复现)。
    #[derive(Default)]
    struct Props {
        href: Option<String>,
        etag: Option<String>,
        size: Option<u64>,
        modified: Option<String>,
        is_collection: bool,
    }

    let mut out: Vec<RemoteNode> = Vec::new();
    let mut cur = Props::default();

    let mut in_href = false;
    let mut in_etag = false;
    let mut in_size = false;
    let mut in_modified = false;
    let mut buf = Vec::new();

    // 把 cur 收尾成一个节点(若是文件或是目录),然后清空
    macro_rules! flush {
        () => {{
            let p = std::mem::take(&mut cur);
            if let Some(h) = p.href {
                if let Some(rel) = href_to_rel(&h, root) {
                    let rel = rel.trim_end_matches('/').to_string();
                    let self_trim = self_rel.trim_matches('/');
                    // 丢弃:远端根本身、以及所查目录的自引用条目
                    if !rel.is_empty() && rel != self_trim {
                        let name = rel
                            .rsplit('/')
                            .next()
                            .unwrap_or(rel.as_str())
                            .to_string();
                        out.push(RemoteNode {
                            rel_path: rel,
                            name,
                            is_dir: p.is_collection,
                            size: if p.is_collection { None } else { p.size },
                            modified: p.modified,
                            etag: p.etag,
                            children: Vec::new(),
                        });
                    }
                }
            }
        }};
    }

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) => {
                if local_name(e.name().as_ref()) == b"collection" {
                    cur.is_collection = true;
                }
            }
            Ok(Event::Start(e)) => match local_name(e.name().as_ref()) {
                b"href" => in_href = true,
                b"getetag" => in_etag = true,
                b"getcontentlength" => in_size = true,
                b"getlastmodified" => in_modified = true,
                b"collection" => cur.is_collection = true,
                _ => {}
            },
            Ok(Event::Text(t)) => {
                let s = t.unescape().unwrap_or_default().trim().to_string();
                if s.is_empty() {
                    buf.clear();
                    continue;
                }
                if in_href && cur.href.is_none() {
                    cur.href = Some(s);
                } else if in_etag && cur.etag.is_none() {
                    cur.etag = Some(s);
                } else if in_size && cur.size.is_none() {
                    cur.size = parse_size(&s);
                } else if in_modified && cur.modified.is_none() {
                    cur.modified = Some(s);
                }
            }
            Ok(Event::End(e)) => match local_name(e.name().as_ref()) {
                b"href" => in_href = false,
                b"getetag" => in_etag = false,
                b"getcontentlength" => in_size = false,
                b"getlastmodified" => in_modified = false,
                b"response" => flush!(),
                _ => {}
            },
            Ok(Event::Eof) => {
                flush!();
                break;
            }
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    // 稳定排序:目录在前,然后按名字
    out.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    out
}

/// 目录树拉取的最大深度。
///
/// 防止远端结构异常(比如自引用)时无限递归。工程的目录层级是
/// `工程/分组/文件`,4 层足够。
pub const MAX_TREE_DEPTH: usize = 4;

/// 递归拉取远端目录树。
///
/// # 为什么是逐层 PROPFIND 而不是 `Depth: infinity`
///
/// 一次 `Depth: infinity` 理论上更快,但**不是所有 WebDAV 服务器都支持**
/// (Seafile 的实现未必),而且失败时拿不到任何部分结果。逐层拉取慢一些,
/// 但每一层失败了都还能给出已经拿到的部分 —— 对"看清云端有什么"这个
/// 目的来说,部分结果比整体失败有用。
///
/// # 为什么是迭代而不是递归
///
/// 递归的 `async fn` 在 Rust 里编译不过(返回的 Future 大小无法确定,
/// 报 `E0733: recursion in an async fn requires boxing`)。`Box::pin` 能绕,
/// 但这里用显式的栈更直观,也顺手避开了借用问题。
/// 用绝对层级拉取目录树。
///
/// # `max_depth` 的语义
///
/// **`max_depth = N` 表示"根 + 往下 N 层文件"**,即总共拉 N+1 层目录。
///
/// 这是用户直觉上的"深度":说"拉 3 层"时想的是
/// `projects/<工程>/audio/<这里的文件>`,而不是"只拉到目录名为止"。
///
/// 实现上循环多跑一轮(`level <= max_depth`),所以
/// `max_depth = 3` 会拉 level 0~3 四层目录 —— 足以拿到最深层目录里的文件。
///
/// # 为什么逐层 PROPFIND 而不是 `Depth: infinity`
///
/// 一次 `Depth: infinity` 理论上更快,但**不是所有 WebDAV 服务器都支持**
/// (Seafile 的实现未必),而且失败时拿不到任何部分结果。逐层拉取慢一些,
/// 但每一层失败了都还能给出已经拿到的部分 —— 对"看清云端有什么"这个
/// 目的来说,部分结果比整体失败有用。
///
/// # 为什么同一层并发
///
/// 串行在深树上是 O(层数 × 单次延迟)。实测每次 PROPFIND 约 130ms,
/// 十层就是 1.3 秒,界面能感觉出来。
pub async fn fetch_tree(
    client: &WebDavClient,
    rel_dir: &str,
    max_depth: usize,
) -> Result<Vec<RemoteNode>> {
    // ★ 用**绝对层级**而不是递减的剩余深度。
    //
    // 之前的写法是 `queue.push((child, depth - 1))`,`depth` 从 max_depth 开始
    // 往下减。看代码像是"还能往下走几层",实际效果是 `max_depth` 变成了
    // **总目录层数**,最深层目录的**内容**永远拿不到:
    //
    //   max_depth = 3
    //     根("")                          level 0  ✅ 拉
    //     projects                        level 1  ✅ 拉
    //     projects/<工程>                  level 2  ✅ 拉
    //     projects/<工程>/audio            level 3  ❌ 跳过 —— 文件全在它里面
    //
    // 现象是"audio 目录看起来是空的",而它其实有 151 MB 的音频。
    let root = rel_dir.trim_matches('/').to_string();

    // 扁平表:目录 rel_path → 子节点。最后自底向上拼树。
    let mut by_dir: std::collections::BTreeMap<String, Vec<RemoteNode>> =
        std::collections::BTreeMap::new();

    let mut level = 0usize;
    let mut current: Vec<String> = vec![root.clone()];

    // <= 而不是 <:多拉一轮,把最深那层目录的**内容**也拿到
    while level <= max_depth && !current.is_empty() {
        let futs = current.iter().map(|d| {
            let d = d.clone();
            async move {
                let r = client.list_nodes(&d).await;
                (d, r)
            }
        });
        let results = join_all(futs).await;

        let mut next: Vec<String> = Vec::new();
        for (d, res) in results {
            match res {
                Ok(entries) => {
                    for n in &entries {
                        if n.is_dir {
                            next.push(n.rel_path.clone());
                        }
                    }
                    by_dir.insert(d, entries);
                }
                Err(e) => {
                    // 根目录失败要报错;子目录失败只跳过 ——
                    // 部分结果好过整体失败
                    if d == root {
                        return Err(e).with_context(|| format!("列远端目录失败: {d}"));
                    }
                    tracing::debug!("跳过拉取失败的子目录 {d}: {e}");
                }
            }
        }
        current = next;
        level += 1;
    }

    Ok(assemble(&mut by_dir, &root))
}

/// 把扁平表拼成树。
///
/// 自底向上:先处理层级最深的目录,这样拼父层时子层已经就绪。
///
/// # 排序键必须是"路径段数",不是"斜杠个数"
///
/// 踩过的坑:用 `d.matches('/').count()` 当深度,**根目录 `""` 也是 0**,
/// 和 `projects`(同样 0 个斜杠)同级。于是排序把它们混在一起,
/// 根目录被排到了 `projects` **前面** —— 而 `projects` 必须先拼好。
///
/// 后果很隐蔽:树看起来是完整的(子目录都在),但**文件全丢了**,
/// 界面显示"仅本地"。因为根层拼的时候 `built["projects"]` 还不存在,
/// 那句 `if let Some(kids)` 静默跳过,子目录的 `children` 就一直是空的。
///
/// 改用段数:`""` → 0,`projects` → 1,`a/b` → 2。这样根严格排在最后。
fn assemble(
    by_dir: &mut std::collections::BTreeMap<String, Vec<RemoteNode>>,
    root: &str,
) -> Vec<RemoteNode> {
    // 深度 = 路径段数。空路径是根,深度 0。
    fn depth_of(p: &str) -> usize {
        if p.is_empty() {
            0
        } else {
            p.matches('/').count() + 1
        }
    }

    let mut dirs: Vec<String> = by_dir.keys().cloned().collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(depth_of(d)));

    // 已拼好的子树:rel_path → 节点列表
    let mut built: std::collections::BTreeMap<String, Vec<RemoteNode>> =
        std::collections::BTreeMap::new();

    for d in dirs {
        let Some(children) = by_dir.get(&d) else {
            continue;
        };
        let nodes: Vec<RemoteNode> = children
            .iter()
            .map(|n| {
                let mut n = n.clone();
                if n.is_dir {
                    if let Some(kids) = built.get(&n.rel_path) {
                        n.children = kids.clone();
                    }
                }
                n
            })
            .collect();
        built.insert(d, nodes);
    }

    built.remove(root).unwrap_or_default()
}

/// 极简的 `join_all`:不引 futures crate,只做"全部发起、全部收齐"。
///
/// 用 `FuturesUnordered` 也行,但那要引 `futures` 依赖;这里手写一个
/// 顺序 poll 的版本就够了 —— 并发度是同一层的目录数(个位数),
/// 不需要更复杂的调度。
async fn join_all<F, T>(futs: impl IntoIterator<Item = F>) -> Vec<T>
where
    F: std::future::Future<Output = T>,
{
    // 先全部 pin 住,再逐个 await —— 这样请求是**同时发出的**
    let mut pinned: Vec<std::pin::Pin<Box<F>>> =
        futs.into_iter().map(Box::pin).collect();
    let mut out = Vec::with_capacity(pinned.len());
    for f in pinned.iter_mut() {
        out.push(f.as_mut().await);
    }
    out
}

// ---------------------------------------------------------------------------
// 本地 ↔ 云端 差异
// ---------------------------------------------------------------------------

/// 一个文件在本地与云端的对照状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffSide {
    /// 两边都有,且内容哈希一致
    Same,
    /// 两边都有,但内容不同
    Differ,
    /// 只有本地有
    LocalOnly,
    /// 只有云端有
    RemoteOnly,
}

/// 单文件的对照结果。
#[derive(Clone, Debug, Serialize)]
pub struct DiffItem {
    pub rel_path: String,
    pub name: String,
    pub status: DiffSide,
    pub is_dir: bool,
    pub local_size: Option<u64>,
    pub remote_size: Option<u64>,
    pub remote_modified: Option<String>,
    /// 本地相对云端:true 表示本地更新
    pub local_newer: Option<bool>,
}

/// 差异对照树的节点。
#[derive(Clone, Debug, Serialize)]
pub struct DiffNode {
    pub rel_path: String,
    pub name: String,
    pub is_dir: bool,
    /// 该子树里"需要注意"的文件数(状态不是 Same 的)
    pub attention: usize,
    pub status: DiffSide,
    pub local_size: Option<u64>,
    pub remote_size: Option<u64>,
    pub remote_modified: Option<String>,
    /// 该子树里两边都有但内容不同的文件数
    pub differing: usize,
    /// 该子树里只在本地有的文件数
    pub local_only: usize,
    /// 该子树里只在云端有的文件数
    pub remote_only: usize,
    pub children: Vec<DiffNode>,
}

/// 把本地文件表与远端节点树合并成对照树。
///
/// `local` 是 `rel_path → (size, hash)`;`hash` 用于判断"内容是否一致" ——
/// 只比大小会把"改了一个字"当成相同。
pub fn build_diff(
    local: &std::collections::BTreeMap<String, (u64, Option<String>)>,
    remote: &[RemoteNode],
) -> Vec<DiffNode> {
    // 先按路径前缀把远端摊平成 map,便于与本地对齐
    let mut remote_flat: std::collections::BTreeMap<String, RemoteNode> =
        std::collections::BTreeMap::new();
    fn walk(n: &RemoteNode, out: &mut std::collections::BTreeMap<String, RemoteNode>) {
        out.insert(n.rel_path.clone(), n.clone());
        for c in &n.children {
            walk(c, out);
        }
    }
    for n in remote {
        walk(n, &mut remote_flat);
    }

    // 收集所有出现的路径(不含纯目录的父级 —— 那些由目录节点本身表示)
    let mut all: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (k, _) in local.iter() {
        all.insert(k.clone());
    }
    for (k, n) in remote_flat.iter() {
        if !n.is_dir {
            all.insert(k.clone());
        }
    }

    // 只保留文件;目录由 build_tree 从路径推导
    let mut file_nodes: Vec<DiffNode> = Vec::new();
    for rel in all {
        let l = local.get(&rel);
        let r = remote_flat.get(&rel);
        let r_is_dir = r.map(|n| n.is_dir).unwrap_or(false);
        // 远端把某个路径当目录、本地当文件 → 冲突,按"两边都有但不同"处理
        let status = match (l.is_some(), r.is_some() && !r_is_dir) {
            (true, true) => {
                let lsize = l.map(|(s, _)| *s);
                let rsize = r.and_then(|n| n.size);
                if rsize == lsize {
                    // 大小一致就认为一致 —— 远端 ETag 的算法各家不同
                    // (Seafile 是内容哈希,别的服务器可能是 mtime+size),
                    // 不能直接拿它和本地内容哈希比。
                    DiffSide::Same
                } else {
                    DiffSide::Differ
                }
            }
            (true, false) => DiffSide::LocalOnly,
            (false, true) => DiffSide::RemoteOnly,
            (false, false) => continue,
        };

        let lsize = l.map(|(s, _)| *s);

        file_nodes.push(DiffNode {
            name: rel.rsplit('/').next().unwrap_or(&rel).to_string(),
            rel_path: rel,
            is_dir: false,
            attention: 0,
            status,
            local_size: lsize,
            remote_size: r.and_then(|n| n.size),
            remote_modified: r.and_then(|n| n.modified.clone()),
            differing: 0,
            local_only: 0,
            remote_only: 0,
            children: Vec::new(),
        });
    }

    nest(file_nodes)
}

/// 把扁平的文件列表嵌成树,并自底向上聚合统计。
fn nest(files: Vec<DiffNode>) -> Vec<DiffNode> {
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Dir {
        children: BTreeMap<String, Dir>,
        files: Vec<DiffNode>,
    }

    let mut root = Dir::default();
    for f in files {
        let parts: Vec<&str> = f.rel_path.split('/').collect();
        let mut cur = &mut root;
        for p in &parts[..parts.len().saturating_sub(1)] {
            cur = cur.children.entry(p.to_string()).or_default();
        }
        cur.files.push(f);
    }

    fn finalize(name: &str, prefix: &str, d: Dir) -> Vec<DiffNode> {
        let mut out: Vec<DiffNode> = Vec::new();

        for (cname, cdir) in d.children {
            let cprefix = if prefix.is_empty() {
                cname.clone()
            } else {
                format!("{prefix}/{cname}")
            };
            let mut kids = finalize(&cname, &cprefix, cdir);
            let (att, diff, lo, ro) = rollup(&kids);
            out.push(DiffNode {
                rel_path: cprefix.clone(),
                name: cname,
                is_dir: true,
                attention: att,
                status: if att == 0 {
                    DiffSide::Same
                } else {
                    DiffSide::Differ
                },
                local_size: None,
                remote_size: None,
                remote_modified: None,
                differing: diff,
                local_only: lo,
                remote_only: ro,
                children: std::mem::take(&mut kids),
            });
        }

        let mut fs = d.files;
        fs.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend(fs);
        let _ = name;
        out
    }

    fn rollup(nodes: &[DiffNode]) -> (usize, usize, usize, usize) {
        let mut att = 0;
        let mut diff = 0;
        let mut lo = 0;
        let mut ro = 0;
        for n in nodes {
            if n.is_dir {
                att += n.attention;
                diff += n.differing;
                lo += n.local_only;
                ro += n.remote_only;
            } else {
                match n.status {
                    DiffSide::Same => {}
                    DiffSide::Differ => {
                        att += 1;
                        diff += 1;
                    }
                    DiffSide::LocalOnly => {
                        att += 1;
                        lo += 1;
                    }
                    DiffSide::RemoteOnly => {
                        att += 1;
                        ro += 1;
                    }
                }
            }
        }
        (att, diff, lo, ro)
    }

    finalize("", "", root)
}

// ---------------------------------------------------------------------------
// 手动操作:删除、单文件上传/下载
// ---------------------------------------------------------------------------

/// 删除云端的一个文件或目录。
///
/// 目录会**递归删除**(先列子项再逐个删,最后删目录本身)。
///
/// # 为什么不直接用 `Depth: infinity`
///
/// WebDAV 规范的 DELETE 默认就是递归的,但:
///
/// 1. **不是所有服务器都支持** —— 实测里 Seafile 这类实现未必，
///    失败时报的错也含糊(可能只是 403)。
/// 2. 递归删之前先列出来,可以**先统计要删多少**,让调用方有机会确认。
///
/// 所以这里显式递归。代价是请求多一些,但对"删一个工程目录"这个量级
/// (几十个文件)完全可接受,换来的是确定的行为和可预期的错误。
///
/// # 安全
///
/// 这是**用户主动发起**的操作,不写墓碑、不查删除策略。理由见模块文档:
/// 墓碑的语义是"把删除同步给别的设备",而手动删除是"我不想要云端这一份",
/// 两者不该混。调用方负责确认。
pub async fn delete_node(client: &WebDavClient, node: &RemoteNode) -> Result<DeleteReport> {
    let mut report = DeleteReport::default();

    if node.is_dir {
        delete_dir_recursive(client, &node.rel_path, &mut report).await?;
        // 目录本身最后删
        client
            .delete(&node.rel_path)
            .await
            .with_context(|| format!("删除目录失败: {}", node.rel_path))?;
        report.dirs_deleted += 1;
    } else {
        client
            .delete(&node.rel_path)
            .await
            .with_context(|| format!("删除文件失败: {}", node.rel_path))?;
        report.files_deleted += 1;
        report.bytes_deleted += node.size.unwrap_or(0);
    }

    Ok(report)
}

/// 删除结果的统计。
#[derive(Clone, Debug, Default, Serialize)]
pub struct DeleteReport {
    pub files_deleted: usize,
    pub dirs_deleted: usize,
    pub bytes_deleted: u64,
    /// 删不掉的条目(权限、被占用等),用于提示"部分失败"
    pub failed: Vec<String>,
}

/// 递归删目录内容。**逐层往下,先删文件再删子目录。**
///
/// 单条失败**不中断** —— 记进 `failed` 继续删剩下的。
/// 理由:删一半停下来会留下更难收拾的中间状态,不如尽量删完再报告。
async fn delete_dir_recursive(
    client: &WebDavClient,
    rel_dir: &str,
    report: &mut DeleteReport,
) -> Result<()> {
    let entries = client.list_nodes(rel_dir).await?;
    for e in entries {
        if e.is_dir {
            if let Err(err) = Box::pin(delete_dir_recursive(client, &e.rel_path, report)).await {
                report.failed.push(format!("{}: {err}", e.rel_path));
                continue;
            }
            match client.delete(&e.rel_path).await {
                Ok(()) => report.dirs_deleted += 1,
                Err(err) => report.failed.push(format!("{}: {err}", e.rel_path)),
            }
        } else {
            match client.delete(&e.rel_path).await {
                Ok(()) => {
                    report.files_deleted += 1;
                    report.bytes_deleted += e.size.unwrap_or(0);
                }
                Err(err) => report.failed.push(format!("{}: {err}", e.rel_path)),
            }
        }
    }
    Ok(())
}

/// 把本地一个文件单独传上云端(不走同步引擎、不写清单)。
///
/// 用于"我只想同步这一个文件"的场景 —— 比如刚改完的总结想立刻传上去,
/// 但不想跑整轮同步。
///
/// **不写墓碑、不改 `sync-state`** —— 保持"手动"与"自动"两条路径分离,
/// 手动操作不干扰同步引擎对状态的判断。
pub async fn upload_one(client: &WebDavClient, local: &std::path::Path, rel: &str) -> Result<u64> {
    let data = std::fs::read(local)
        .with_context(|| format!("读取本地文件失败: {}", local.display()))?;
    let n = data.len() as u64;
    client
        .put_atomic(rel, &data)
        .await
        .with_context(|| format!("上传失败: {rel}"))?;
    Ok(n)
}

/// 从云端拉一个文件覆盖本地。
///
/// 会自动建本地父目录。云端没有这个文件时返回 `Ok(None)`。
pub async fn download_one(
    client: &WebDavClient,
    rel: &str,
    local: &std::path::Path,
) -> Result<Option<u64>> {
    let Some(data) = client.get(rel).await? else {
        return Ok(None);
    };
    if let Some(parent) = local.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建本地目录失败: {}", parent.display()))?;
    }
    let n = data.len() as u64;
    std::fs::write(local, &data)
        .with_context(|| format!("写入本地文件失败: {}", local.display()))?;
    Ok(Some(n))
}

/// 拉取远端树并与本地文件表做对照。
///
/// `local` 通常是 [`crate::store::files::FileStore`] 遍历出来的
/// `rel_path → (size, hash)`。
pub async fn fetch_diff(
    client: &WebDavClient,
    local: &std::collections::BTreeMap<String, (u64, Option<String>)>,
    max_depth: usize,
) -> Result<Vec<DiffNode>> {
    let tree = fetch_tree(client, "", max_depth).await?;
    Ok(build_diff(local, &tree))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/seafdav/recording-summary/</D:href>
    <D:propstat><D:prop>
      <D:resourcetype><D:collection/></D:resourcetype>
    </D:prop></D:propstat>
  </D:response>
  <D:response>
    <D:href>/seafdav/recording-summary/manifest.json</D:href>
    <D:propstat><D:prop>
      <D:resourcetype/>
      <D:getcontentlength>4096</D:getcontentlength>
      <D:getlastmodified>Sat, 11 Sep 2026 12:00:00 GMT</D:getlastmodified>
      <D:getetag>"abc123"</D:getetag>
    </D:prop></D:propstat>
  </D:response>
  <D:response>
    <D:href>/seafdav/recording-summary/projects/</D:href>
    <D:propstat><D:prop>
      <D:resourcetype><D:collection/></D:resourcetype>
    </D:prop></D:propstat>
  </D:response>
</D:multistatus>"#;

    #[test]
    fn keeps_directories_unlike_parse_listing() {
        let nodes = parse_propfind(SAMPLE, "https://pan.ustc.edu.cn/seafdav/recording-summary", "");
        // 自身被丢弃,余下 1 文件 + 1 目录
        assert_eq!(nodes.len(), 2, "{nodes:#?}");
        let dir = nodes.iter().find(|n| n.is_dir).expect("应保留目录");
        assert_eq!(dir.rel_path, "projects");
        assert_eq!(dir.name, "projects");
        assert_eq!(dir.size, None, "目录不该有大小");
    }

    #[test]
    fn parses_size_and_modified() {
        let nodes = parse_propfind(SAMPLE, "https://pan.ustc.edu.cn/seafdav/recording-summary", "");
        let f = nodes.iter().find(|n| !n.is_dir).expect("应有文件");
        assert_eq!(f.rel_path, "manifest.json");
        assert_eq!(f.size, Some(4096));
        assert_eq!(f.etag.as_deref(), Some("\"abc123\""));
        assert_eq!(
            f.modified.as_deref(),
            Some("Sat, 11 Sep 2026 12:00:00 GMT")
        );
    }

    #[test]
    fn drops_the_root_itself() {
        let nodes = parse_propfind(SAMPLE, "https://pan.ustc.edu.cn/seafdav/recording-summary", "");
        assert!(
            !nodes.iter().any(|n| n.rel_path.is_empty()),
            "不该把远端根自己列出来"
        );
    }

    #[test]
    fn dirs_sort_before_files() {
        let nodes = parse_propfind(SAMPLE, "https://pan.ustc.edu.cn/seafdav/recording-summary", "");
        assert!(nodes[0].is_dir, "目录应排在前面:{nodes:#?}");
    }

    #[test]
    fn handles_self_closing_and_paired_collection() {
        // <D:collection/> 是 Event::Empty,<D:collection></D:collection> 是 Start。
        // 两种都必须是目录 —— 漏掉 Empty 会把目录当文件。
        let self_closing = r#"<D:multistatus xmlns:D="DAV:"><D:response>
            <D:href>/r/sub/</D:href><D:propstat><D:prop>
            <D:resourcetype><D:collection/></D:resourcetype>
            </D:prop></D:propstat></D:response></D:multistatus>"#;
        let paired = r#"<D:multistatus xmlns:D="DAV:"><D:response>
            <D:href>/r/sub/</D:href><D:propstat><D:prop>
            <D:resourcetype><D:collection></D:collection></D:resourcetype>
            </D:prop></D:propstat></D:response></D:multistatus>"#;
        for xml in [self_closing, paired] {
            let n = parse_propfind(xml, "/r", "");
            assert_eq!(n.len(), 1);
            assert!(n[0].is_dir, "应识别为目录: {xml}");
        }
    }

    #[test]
    fn tolerates_missing_optional_props() {
        // 有些服务器不给 contentlength / lastmodified,不能因此丢条目
        let xml = r#"<D:multistatus xmlns:D="DAV:"><D:response>
            <D:href>/r/a.txt</D:href>
            <D:propstat><D:prop><D:resourcetype/></D:prop></D:propstat>
            </D:response></D:multistatus>"#;
        let n = parse_propfind(xml, "/r", "");
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].size, None);
        assert_eq!(n[0].modified, None);
        assert!(!n[0].is_dir);
    }

    #[test]
    fn percent_encoded_names_are_decoded() {
        let xml = r#"<D:multistatus xmlns:D="DAV:"><D:response>
            <D:href>/r/%E5%B7%A5%E7%A8%8B/a.txt</D:href>
            <D:propstat><D:prop><D:resourcetype/></D:prop></D:propstat>
            </D:response></D:multistatus>"#;
        let n = parse_propfind(xml, "/r", "");
        assert_eq!(n[0].rel_path, "工程/a.txt");
    }

    /// ★ 实测于 Seafile:PROPFIND 一个目录时,响应里**同时包含这个目录本身**。
    ///
    /// 不过滤的话,界面上每展开一层就会多出一个"同名子目录",
    /// 而它点进去是空的 —— 用户会以为"点不开"。
    /// (真实现象:工程目录里又出现一个 `projects`,点进去什么都没有。)
    #[test]
    fn filters_out_self_reference_entry() {
        let xml = r#"<D:multistatus xmlns:D="DAV:">
          <D:response>
            <D:href>/seafdav/recording-summary/projects</D:href>
            <D:propstat><D:prop>
              <D:resourcetype><D:collection/></D:resourcetype>
            </D:prop></D:propstat>
          </D:response>
          <D:response>
            <D:href>/seafdav/recording-summary/projects/%E5%B7%A5%E7%A8%8BA</D:href>
            <D:propstat><D:prop>
              <D:resourcetype><D:collection/></D:resourcetype>
            </D:prop></D:propstat>
          </D:response>
        </D:multistatus>"#;
        let root = "https://pan.ustc.edu.cn/seafdav/recording-summary";
        let n = parse_propfind(xml, root, "projects");
        assert_eq!(n.len(), 1, "自引用条目应被丢弃:{n:#?}");
        assert_eq!(n[0].rel_path, "projects/工程A");
    }

    #[test]
    fn self_reference_filter_is_exact_not_prefix() {
        // ★ 不能误伤同名前缀的兄弟:列 "a" 时 "ab" 必须保留
        let xml = r#"<D:multistatus xmlns:D="DAV:">
          <D:response><D:href>/r/a</D:href><D:propstat><D:prop>
            <D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>
          <D:response><D:href>/r/ab</D:href><D:propstat><D:prop>
            <D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>
          <D:response><D:href>/r/a.txt</D:href><D:propstat><D:prop>
            <D:resourcetype/></D:prop></D:propstat></D:response>
        </D:multistatus>"#;
        let n = parse_propfind(xml, "/r", "a");
        let names: Vec<&str> = n.iter().map(|x| x.rel_path.as_str()).collect();
        assert!(!names.contains(&"a"), "自己应被丢弃:{names:?}");
        assert!(names.contains(&"ab"), "同名前缀的兄弟必须保留:{names:?}");
        assert!(names.contains(&"a.txt"), "{names:?}");
    }

    #[test]
    fn self_reference_filter_handles_trailing_slash() {
        let xml = r#"<D:multistatus xmlns:D="DAV:">
          <D:response><D:href>/r/sub/</D:href><D:propstat><D:prop>
            <D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>
          <D:response><D:href>/r/sub/x.txt</D:href><D:propstat><D:prop>
            <D:resourcetype/></D:prop></D:propstat></D:response>
        </D:multistatus>"#;
        for slf in ["sub", "sub/", "/sub", "/sub/"] {
            let n = parse_propfind(xml, "/r", slf);
            assert_eq!(n.len(), 1, "self_rel={slf:?} 时应只剩子文件:{n:#?}");
            assert_eq!(n[0].rel_path, "sub/x.txt");
        }
    }

    #[test]
    fn empty_self_rel_keeps_everything_except_root() {
        let xml = r#"<D:multistatus xmlns:D="DAV:">
          <D:response><D:href>/r/</D:href><D:propstat><D:prop>
            <D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>
          <D:response><D:href>/r/a</D:href><D:propstat><D:prop>
            <D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>
        </D:multistatus>"#;
        let n = parse_propfind(xml, "/r", "");
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].rel_path, "a");
    }

    // --- 差异对照 ----------------------------------------------------------

    fn rnode(rel: &str, size: u64) -> RemoteNode {
        RemoteNode {
            name: rel.rsplit('/').next().unwrap().to_string(),
            rel_path: rel.to_string(),
            is_dir: false,
            size: Some(size),
            modified: None,
            etag: None,
            children: Vec::new(),
        }
    }

    fn rdir(rel: &str, kids: Vec<RemoteNode>) -> RemoteNode {
        RemoteNode {
            name: rel.rsplit('/').next().unwrap().to_string(),
            rel_path: rel.to_string(),
            is_dir: true,
            size: None,
            modified: None,
            etag: None,
            children: kids,
        }
    }

    fn local(pairs: &[(&str, u64)]) -> std::collections::BTreeMap<String, (u64, Option<String>)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), (*v, None)))
            .collect()
    }

    #[test]
    fn diff_finds_same_differ_and_one_sided() {
        let l = local(&[("a.txt", 10), ("b.txt", 20), ("only_local.txt", 5)]);
        let r = vec![
            rnode("a.txt", 10),
            rnode("b.txt", 99), // 大小不同 → Differ
            rnode("only_remote.txt", 7),
        ];
        let t = build_diff(&l, &r);

        let find = |name: &str| -> DiffNode {
            fn walk(n: &[DiffNode], name: &str) -> Option<DiffNode> {
                for x in n {
                    if x.name == name {
                        return Some(x.clone());
                    }
                    if let Some(f) = walk(&x.children, name) {
                        return Some(f);
                    }
                }
                None
            }
            walk(&t, name).unwrap_or_else(|| panic!("找不到 {name}"))
        };

        assert_eq!(find("a.txt").status, DiffSide::Same);
        assert_eq!(find("b.txt").status, DiffSide::Differ);
        assert_eq!(find("only_local.txt").status, DiffSide::LocalOnly);
        assert_eq!(find("only_remote.txt").status, DiffSide::RemoteOnly);
    }

    #[test]
    fn diff_nests_into_directories_with_rollup() {
        let l = local(&[
            ("proj/audio/recording.m4a", 100),
            ("proj/summary-brief.md", 10),
        ]);
        let r = vec![rdir(
            "proj",
            vec![
                rdir("proj/audio", vec![rnode("proj/audio/recording.m4a", 100)]),
                // 云端少了 summary-brief.md
            ],
        )];
        let t = build_diff(&l, &r);
        let proj = t.iter().find(|n| n.name == "proj").expect("应有 proj 目录");
        assert!(proj.is_dir);
        assert_eq!(proj.local_only, 1, "summary-brief.md 只在本地");
        assert_eq!(proj.attention, 1);
        let audio = proj
            .children
            .iter()
            .find(|n| n.name == "audio")
            .expect("应有 audio 目录");
        assert_eq!(audio.attention, 0, "audio 下两边一致,不该需要关注");
    }

    #[test]
    fn diff_ignores_identical_trees() {
        let l = local(&[("x/y.txt", 5)]);
        let r = vec![rdir("x", vec![rnode("x/y.txt", 5)])];
        let t = build_diff(&l, &r);
        assert_eq!(t[0].attention, 0);
        assert_eq!(t[0].status, DiffSide::Same);
    }

    // --- 树组装(两个真实 bug 的发生地)----------------------------------

    fn flat(
        pairs: &[(&str, Vec<RemoteNode>)],
    ) -> std::collections::BTreeMap<String, Vec<RemoteNode>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    /// ★ 回归测试:根目录必须**最后**组装。
    ///
    /// bug 成因:排序键用 `d.matches('/').count()` 当深度,根目录 `""` 也是 0,
    /// 和 `projects`(同样 0 个斜杠)同级 —— 根被排到了 `projects` **前面**。
    /// 根先拼时 `built["projects"]` 还不存在,`if let Some(kids)` 静默跳过,
    /// 于是树看起来有子目录但**文件全丢了**,界面显示"仅本地"。
    #[test]
    fn assemble_root_is_processed_last() {
        let mut by_dir = flat(&[
            (
                "",
                vec![rdir("projects", vec![]), rnode("manifest.json", 100)],
            ),
            ("projects", vec![rdir("projects/P", vec![])]),
            (
                "projects/P",
                vec![
                    rdir("projects/P/audio", vec![]),
                    rnode("projects/P/brief.md", 10),
                ],
            ),
            (
                "projects/P/audio",
                vec![rnode("projects/P/audio/rec.m4a", 999)],
            ),
        ]);

        let tree = assemble(&mut by_dir, "");
        assert_eq!(tree.len(), 2, "根层应有两个条目:{tree:#?}");

        let projects = tree
            .iter()
            .find(|n| n.name == "projects")
            .expect("应有 projects");
        assert_eq!(projects.children.len(), 1, "projects 应有 1 个孩子");

        let p = &projects.children[0];
        assert_eq!(p.name, "P");
        assert_eq!(
            p.children.len(),
            2,
            "工程下应有 audio + brief:{:#?}",
            p.children
        );

        let audio = p
            .children
            .iter()
            .find(|n| n.name == "audio")
            .expect("应有 audio 目录");
        assert_eq!(
            audio.children.len(),
            1,
            "★ audio 里的文件必须出现 —— 这正是之前丢掉的那一批"
        );
        assert_eq!(audio.children[0].name, "rec.m4a");
    }

    #[test]
    fn assemble_handles_empty_and_single_level() {
        let mut e = std::collections::BTreeMap::new();
        assert!(assemble(&mut e, "").is_empty());

        let mut one = flat(&[("", vec![rnode("a.txt", 1), rnode("b.txt", 2)])]);
        let t = assemble(&mut one, "");
        assert_eq!(t.len(), 2);
        assert!(t.iter().all(|n| !n.is_dir));
    }

    #[test]
    fn assemble_with_subdir_root() {
        // rel_dir 非空时,返回那一层的节点,且路径带前缀
        let mut by_dir = flat(&[
            (
                "sub",
                vec![rdir("sub/inner", vec![]), rnode("sub/x.txt", 5)],
            ),
            ("sub/inner", vec![rnode("sub/inner/y.txt", 6)]),
        ]);
        let t = assemble(&mut by_dir, "sub");
        assert_eq!(t.len(), 2);
        let inner = t.iter().find(|n| n.name == "inner").expect("应有 inner");
        assert_eq!(inner.children.len(), 1, "inner 下的文件应挂上:{inner:#?}");
    }

    #[test]
    fn totals_counts_files_and_bytes() {
        let d = rdir(
            "p",
            vec![
                rnode("p/a", 10),
                rdir("p/sub", vec![rnode("p/sub/b", 20)]),
            ],
        );
        assert_eq!(d.totals(), (2, 30));
    }
}
