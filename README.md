# 录音转总结

把课堂录音、会议录音变成**带时间戳、带发言人、按场景结构化**的纪要。

转写全程在本机完成,音频不出机器;只有转写后的文字会发给 LLM。所以它既是离线的,LLM 调用成本也可以忽略。

> 界面语言为中文。完整设计文档见 [`docs/技术方案.md`](docs/技术方案.md) —— 约 2700 行,记录了每条设计取舍的理由与实测数据。

---

## 它做什么

拖入一个音频文件,得到一份**工程目录**:

```text
projects/2026-09-11_高等数学第12讲/
├─ project.json          元数据(场景、模型、后端、产物清单)
├─ audio/recording.wav   录音副本 —— 整个目录可以直接打包发人
├─ transcript.md         带时间戳的转写(对话体)
├─ transcript.json       结构化段落
├─ transcript.srt        字幕
├─ summary-detailed.md   详细总结(知识点 / 例题 / 待办 / 复习提纲)
├─ summary-brief.md      简略总结(一句话 + 要点,400 字以内)
├─ mindmap.mmd           思维导图(Mermaid 源码)
└─ mindmap-outline.md    文本大纲(图渲染失败时的降级)
```

两份总结**不是同一份东西的压缩**,而是换了目标:详细版给"要复习、要执行"的人,简略版给"只想回忆讲了什么"的人。层次结构交给思维导图。

---

## 特性

| | |
|---|---|
| **转写在本机** | 音频不出机器,零调用成本,可离线 |
| **两个转写引擎** | whisper.cpp(快)或 CrispASR/FireRedASR2(中文更准,慢 6 倍),按需切换 |
| **自动选后端** | 按 `CUDA → Vulkan → CPU` 顺序探测,不预设你有什么硬件 |
| **说话人区分** | sherpa-onnx 离线聚类 + 段落级重叠投票,可手动指定人数 |
| **声纹档案** | 同一个人第二次出现时自动认出;三级匹配(确认/待定/新建)让错误可见 |
| **中英混合** | 语码转换天然支持;简体提示无条件注入(理由见下) |
| **转写纠错** | 转写后用 LLM 修同音字与术语错误;术语表是权威写法(见下) |
| **场景自适应** | 自动判断课堂/会议/访谈,套用不同的总结模板 |
| **长音频** | 分块转写 + 断点续传;超出上下文时走 map-reduce |
| **工程制** | 每次处理产生一个自包含目录,可打包、可同步、可阅读 |
| **选择性同步** | 方向、范围、音频、删除策略四个独立维度,可精确到单个文件 |
| **云端管理** | 直接浏览云端目录树、本地⇄云端逐文件对照、单独删掉某一项 |
| **单文件同步** | 对照时挑中一个文件,当场决定推上去还是拉下来 |
| **后端可替换** | 切换后端 = 换一个 exe 路径,不需要重新编译主程序 |

---

## 几个不显然的技术选择

理由都来自实测,详细论证见技术方案。

**whisper.cpp 走 sidecar 子进程,不走 FFI。** 换后端只需换一个 exe 路径;子进程崩溃带不走 GUI;不同后端的构建产物互不干扰。代价是进程启动开销,对分钟级音频可以忽略。

**中文精度不够时,换引擎而不是调参数。** 实测同一段口音较重的课堂录音,whisper large-v3 与 [CrispASR](https://github.com/CrispStrobe/CrispASR) 的 FireRedASR2-AED:

| 内容 | whisper large-v3 | FireRedASR2-AED |
|---|---|---|
| 相声 | `说下手` ❌ | **`相声`** ✅ |
| 逗哏 | `逗评` ❌ | **`逗哏`** ✅ |
| 捧哏 | `捧本` ❌ | **`捧哏`** ✅ |
| 计算机 | `这张记忆里` ❌ | **`计算机`** ✅ |
| `I = F(X, Y)` | `f等于fx` ❌ | **`I等于F X Y`** ✅ |
| **5 分钟耗时** | **19.8 秒** | 120 秒(慢 6 倍) |

**注意 whisper 更大(1550M vs 1100M)却输了。** 决定性的不是参数量,是**分给中文的容量**:whisper 把 1550M 摊到 99 种语言,还要额外承担自回归解码的复杂度;FireRedASR2 把 1100M 几乎全押在中文上,并用更省参数的 CTC 结构。

所以引擎是**可选**的:默认 whisper(2 小时录音约 10 分钟),重要录音或口音重时切 FireRedASR2(约 50 分钟)。

**说话人区分用 sherpa-onnx,不用 pyannote。** 后者要带一个 Python 运行时,而这是要分发给普通用户的桌面程序。

**段落级重叠投票,不用词级时间戳。** whisper.cpp 的 `--max-len 1` 在 CJK 上工作得很差。改用分块重叠 + 投票,精度够用且没有 CJK 问题。

**简体提示无条件注入 `initial_prompt`,不按语言判断。** Whisper 在中文音频上有稳定的繁体偏好(实测"同學們""神經網絡"),而**语言检测不等于内容语言** —— 一门英文术语密集的课会被判成 `en`,中文部分照样要出简体。按 `language.starts_with("zh")` 判断会让"自动检测"这个最适合混合内容的选项拿不到保护。

**同音字靠转写后纠错,不靠热词。** Whisper 的中文错误是**稳定的**:`计算机视觉 → 坚立视觉`、`相机 → 像机`、`二叉树 → 二差数`。实测一个两小时录音里 `坚立视觉` 出现 8 次、`像机` 8 次。热词注入(`initial_prompt`)对这种错误效果很弱 —— 它只是解码期的软偏置。而"同一错误重复出现"恰恰是上下文纠错最擅长的。

所以转写后会再用 LLM 过一遍,并把 `--term` 给的术语表当作**权威写法**(不是"仅供参考" —— 措辞差别实测有影响:说成"参考"时 LLM 把 `坚歷視覺` 改成了毫无意义的 `建立视觉`)。

纠错**直接改写**转写文件,所以有一整套"宁可漏改不可错改"的校验:逐段处理、返回段数不符就整批放弃、单段长度突变 3 倍以上也放弃。任一批失败只记录,不影响其他批,也不让整条管线失败。时间戳不受影响 —— 只替换文本。

实测效果:

```
纠错前:  坚歷視覺的目的是從圖向中挖覺信息我們需要一臺向進來採集數據二差數的便利方式有三種
纠错后:  计算机视觉的目的是从图像中挖掘信息我们需要一台相机来采集数据二叉树的遍历方式有三种
```

**文件是真相源,SQLite 是可重建的缓存。** 数据库坏了就重建,于是同步时不需要处理数据库合并冲突。

**资源路径在多处搜索,不用裸相对路径。** 双击 exe 启动时工作目录可能是 `C:\Windows`,`models/` 和 `binaries/` 会找不到。见 `src-core/src/paths.rs`。

**子进程一律用 `CREATE_NO_WINDOW` 启动。** GUI 程序(PE Subsystem = 2)启动控制台程序时 Windows 会分配一个控制台窗口 —— 表现为启动时闪几个黑框。见 `src-core/src/process.rs`。

---

## 环境要求

| | |
|---|---|
| 操作系统 | Windows(目前只在 Windows 11 上验证过) |
| Rust | 1.75+ |
| 构建依赖 | MSVC 工具链 + LLVM(`libclang` 是 sherpa-rs 的构建依赖) |
| GPU | 可选。CUDA / Vulkan 都行,没有就跑 CPU |

实测环境:

```text
CPU      Intel Core i9-14900HX(32 核)
GPU      NVIDIA GeForce RTX 4060 Laptop(8 GB)
后端     CUDA 12.6(自编译 whisper.cpp,compute capability 8.9)
模型     large-v3-turbo(1549 MB)—— 18 秒音频约 5 秒转完
ffmpeg   9.0.1
GUI      Tauri 2.11.5 + WebView2
```

---

## 快速开始

### 1. 构建

```powershell
git clone https://github.com/yang-zi-xuan/recording-summary.git
cd recording-summary
.\scripts\cargo.ps1 build
```

`scripts/cargo.ps1` 会加载 MSVC 环境并设置 `LIBCLANG_PATH`、`SHERPA_LIB_PATH`。直接 `cargo build` 通常也能过,但这个包装脚本更省事。

### 2. 模型

放到 `models/`,按硬件选档:

```powershell
$base = "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main"

# base(141 MB,CPU 上接近实时)
curl.exe -L -o models/ggml-base.bin "$base/ggml-base.bin"

# small(465 MB,CPU 推荐档,质量明显更好)
curl.exe -L -o models/ggml-small.bin "$base/ggml-small.bin"

# large-v3-turbo(1549 MB)—— 快,GPU 上默认用它
curl.exe -L -o models/ggml-large-v3-turbo.bin "$base/ggml-large-v3-turbo.bin"

# large-v3(2952 MB)—— 慢约 1.6 倍但中文明显更准
curl.exe -L -o models/ggml-large-v3.bin "$base/ggml-large-v3.bin"
```

**`turbo` 是 `large-v3` 的蒸馏版**(809M vs 1550M 参数,只有一半)。中文场景下差别是实的 —— 实测同一段 2 小时录音,`坚立视觉` 之类的错误在 turbo 里出现 8 次,在 large-v3 里 **0 次**。

说话人区分还需要两个 ONNX 模型(共 43 MB),见[技术方案 §7](docs/技术方案.md)。

### 3. whisper.cpp 二进制

放到 `binaries/<后端>/whisper-cli.exe`,例如 `binaries/cpu/`。**需要与 exe 同目录的 DLL**(`whisper.dll`、`ggml*.dll`)。

CUDA 版还要 `cudart64_12.dll` / `cublas64_12.dll` / `cublasLt64_12.dll`。

编译方法见[编译 whisper.cpp](#编译-whispercpp)。

### 4. 中文引擎(可选)

默认的 whisper 在中文上够用但不算最优。要更高精度就装 CrispASR:

```powershell
# ① 二进制(约 136 MB)。用 -non-cuda 包是因为 CUDA 运行时 DLL
#    已经随 whisper.cpp 的 CUDA 版放在 binaries/cuda/ 了
curl.exe -L -o binaries\crispasr\crispasr.zip `
  https://github.com/CrispStrobe/CrispASR/releases/download/v0.8.32/crispasr-windows-x86_64-cuda-non-cuda.zip
Expand-Archive binaries\crispasr\crispasr.zip -DestinationPath binaries\crispasr

# ② 把 CUDA 运行时 DLL 拷进 CrispASR 目录(保持它自包含)
Copy-Item binaries\cuda\cudart64_12.dll,binaries\cuda\cublas64_12.dll,binaries\cuda\cublasLt64_12.dll binaries\crispasr\crispasr-windows-*\ -Force

# ③ 模型(919 MB)
curl.exe -L -o models\firered\firered-asr2-aed-q4_k.gguf `
  https://hf-mirror.com/cstr/firered-asr2-aed-GGUF/resolve/main/firered-asr2-aed-q4_k.gguf

# ④ 标点模型(55 MB)。默认从 huggingface.co 下载,那个域名在国内不通,
#    所以手动从镜像取到它期望的位置
curl.exe -L -o "$env:USERPROFILE\.cache\crispasr\fireredpunc-q4_k.gguf" `
  https://hf-mirror.com/cstr/fireredpunc-GGUF/resolve/main/fireredpunc-q4_k.gguf
```

装好后:

```powershell
rs run 录音.m4a --engine firered --term "计算机视觉,双边滤波"
```

GUI 里「处理录音」页有「转写引擎」下拉框,默认 `whisper(快)`。

**不用 CrispASR 也不影响其他功能** —— 它是独立的 exe,只在选中对应引擎时才去找。

### 5. 前端依赖

思维导图用 mermaid(5.4 MB),不进版本库:

```powershell
curl.exe -L --ssl-no-revoke -o ui\vendor\mermaid.min.js `
  https://registry.npmmirror.com/mermaid/latest/files/dist/mermaid.min.js
```

**没有它程序照常运行** —— 只是思维导图标签页会降级显示文本大纲,并在状态栏说明原因。

### 6. 配置 LLM

不配也能用:会完成转写和说话人区分,只是没有总结和思维导图。

```powershell
.\target\debug\rs.exe config set-key sk-xxxxxxxx   # 存进 Windows 凭据管理器
.\target\debug\rs.exe config test                  # 验证连通性
```

API Key 与 WebDAV 密码都存在**系统凭据管理器**,不写进任何配置文件。

### 7. 跑一段录音

```powershell
.\target\debug\rs.exe run 录音.m4a --language zh --print
```

处理完会自动建工程。用 `rs projects` 查看,或打开 GUI 的「我的工程」。

想要桌面快捷方式:

```powershell
.\scripts\create-desktop-shortcut.cmd
```

---

## 命令一览

| 命令 | 说明 |
|---|---|
| `rs probe` | 硬件探测与配置自检 |
| `rs run <文件>` | 完整管线:解码 → 转写 → 说话人 → 场景 → 建工程 |
| `rs projects` | 列出全部工程(含产物完整度) |
| `rs projects show <ID>` | 工程详情 |
| `rs projects cat <ID> brief` | 打印产物(`brief`/`detailed`/`transcript`/`srt`/`mindmap`/`outline`) |
| `rs projects export <ID> <目录>` | 整个工程拷出去 |
| `rs projects rename <ID> <新标题>` | 重命名工程(标题与目录名一起改) |
| `rs projects delete <ID>` | 删除工程(历史记录与云端副本保留) |
| `rs show <会话ID>` | 显示转写(`--view dialogue`/`timeline`/`plain`/`srt`) |
| `rs rename <会话ID> <序号> <名字>` | 改发言人名字(不重新转写,秒级) |
| `rs profiles` | 声纹档案管理 |
| `rs sync [plan\|run]` | 云端同步 |
| `rs sync show --env` | 输出 `KEY=VALUE` 形式的配置(⚠️ 含明文密码,仅诊断用) |
| `rs config data-dir` | 查看/迁移数据目录 |
| `rs stats` | 缓存统计 |

### 工程重命名

```powershell
rs projects rename 1fee8945 "图像处理第一讲"
```

**标题和目录名一起改** —— 只改一个会让界面显示的名字和资源管理器里的目录长期不一致,反而更难找。GUI 里工程详情页有「改名」按钮。

几个刻意的行为:

- **保留日期前缀**。`2026-09-11_` 是**创建日期**不是修改日期;不保留的话每改一次名,这个工程在时间排序里就跳到最前面。
- **工程 ID 不变**。ID 是音频内容哈希,与名字无关,所以转写缓存、声纹标签、同步清单里按 ID 索引的东西全部照旧,不会重跑。
- **撞名不覆盖**。新名字撞上已有工程时自动加 `_2` 后缀。
- **会改变云端路径**。如果这个工程已经同步过,旧路径下的文件在云端会变成孤儿 —— 命令和 GUI 都会明确提醒。
- **会同步历史记录的标题**。标题在两处各存一份(见下),只改一处会让同一个录音在两个页面显示不同的名字。

### 工程删除

```powershell
rs projects delete 1fee8945            # 交互确认
rs projects delete 1fee8945 --yes      # 跳过确认
rs projects delete 1fee8945 --yes --source   # 连原始录音一起删
```

GUI 里工程详情页有「删除」按钮。

**删掉**:工程目录(含音频副本)+ 按内容哈希命名的产物缓存
**保留**:

| | 为什么 |
|---|---|
| **历史记录** | 它记录了"这段录音处理过",工程只是它的一个视图。删掉后历史里那一条显示为「工程已删除」 |
| **云端副本** | 删除只在本地生效。自动删云端会让"手滑删本地"变成"云端也没了" —— 云端要在「云端管理」里显式删 |
| **原始录音** | 它在应用存储目录之外(你的 Downloads 之类)。删除别人的文件不该是默认行为,要删得显式加 `--source` |
| **声纹档案** | 它是全局的、跨会话存在的,不属于某一个工程 |

**共享保护**:产物文件按**音频内容哈希**命名,同一个音频只存一份。所以删除前会检查是否还有别的工程用同一个 ID —— 有的话那些文件保留,并明确报告保留了几个。

### 工程与历史记录是两套存储

这不是实现细节,而是会影响你看到什么:

| | 我的工程 | 历史记录 |
|---|---|---|
| 存在哪 | **文件系统** `store/projects/<日期>_<标题>/` | **SQLite** `cache.db` 的 `sessions` 表 |
| 存什么 | `project.json` + 全部产物 | 标题、时长、场景、状态 |
| 谁是真相源 | ✅ **工程** | 索引,可从文件重建 |

两者用**同一个 ID**(音频内容哈希)关联 —— 历史记录的详情页靠它去文件系统读转写和总结。

因此:**删掉工程目录,历史记录不会跟着消失**(会标记为「工程已删除」);反过来删掉 `cache.db`,工程和产物都还在,只是历史记录空了。这是刻意的 —— 数据库设计上就是可重建的缓存。

### `rs run` 常用参数

```text
--language zh            语言(默认自动检测;简体提示始终注入)
--model small            模型档位(tiny/base/small/medium/large-v3-turbo)
--backend cuda           强制后端(默认自动探测;失败不静默降级)
--speakers 4             指定发言人数(最准;不给则自动检测)
--no-diarize             跳过说话人区分
--no-summary             只转写,不生成总结
--term 反向传播          术语表。既作解码期的热词,也作纠错的权威写法
--print                  跑完立刻打印简略总结
```

### 同步的四个维度

```powershell
# 方向:both(默认)| upload | download
rs sync run --direction download

# 范围:按工程,或按通配符
rs sync run --project 2026-09
rs sync run --include "projects/2026-09*" --exclude "*/audio/*"

# 音频是否参与:sync(默认)| skip
rs sync run --audio skip

# 本地删除时:keep | text(默认)| mirror
rs sync run --deletion text
```

**默认策略**:文本双向镜像,录音只增不减。理由 —— 文本小,本地整理过就该同步上去;录音大,本地删掉通常是腾空间,不该连云端一起清掉。

**安全保证**:删除只在"本机同步过的文件、本地又没了、且在同步范围内"时才传播。别的设备上传但本机还没下载的文件,以及范围外的文件,**永远不会被误删**。

### 云端管理

规则化同步解决不了所有事 —— 有时你就是想看看云端到底有什么,或者单独处理一个文件。所以有一个直接操作云端的入口:

| 动作 | 说明 |
|---|---|
| **浏览云端** | 展开云端目录树,带大小与修改时间 |
| **与本地对照** | 逐文件比对,标出 `一致` / `大小不同` / `仅本地` / `仅云端` |
| **删除** | 删掉云端的某个文件或整个目录(递归),不可恢复 |
| **单文件同步** | 对照时对不一致的文件,当场选 ↑上传 或 ↓下载 |

对照里,状态不一致的文件会带同步按钮。方向由你定 —— 程序只给一个**建议**(两边都有但大小不同时,建议较大的那份,因为编辑一般是在原稿上加东西),另一个选择始终在:

```
📄 transcript.md   本地 100.9 KB / 云端 98.2 KB   [大小不同]  [↑ 上传] [↓ 下载 建议]
```

两个方向都会**覆盖**对面那一份,所以点下去会先确认,并明确写出哪边会被覆盖。

**手动操作不碰 manifest。** 单文件同步和删除都不会写 manifest,也不写本机状态表 —— manifest 是跨设备的合并依据,手写它可能让别的设备误判。代价是下次整批同步会再传一次(内容一致,只是多一个请求),比污染合并依据小得多。

---

## 项目结构

```
recording-summary/
├─ docs/技术方案.md        完整设计文档(取舍、实测数据、踩坑记录)
├─ scripts/cargo.ps1       MSVC 环境包装(推荐用它构建)
├─ src-core/               全部业务逻辑
│   ├─ asr.rs              whisper.cpp sidecar
│   ├─ asr.rs              whisper.cpp sidecar
│   ├─ asr_crisp.rs        CrispASR sidecar(FireRedASR2 / Qwen3-ASR / …)
│   ├─ diarize.rs          说话人聚类
│   ├─ sherpa.rs           sherpa-onnx 接入
│   ├─ hardware.rs         后端探测与降级链
│   ├─ audio.rs            解码、重采样、分块
│   ├─ project.rs          工程制存储
│   ├─ llm/                LLM 客户端与提示词
│   ├─ sync/               同步
│   │   ├─ selection.rs    四个维度的选择规则
│   │   ├─ manifest.rs     云端文件清单(append-only + 墓碑)
│   │   ├─ state.rs        本机同步状态(只在本机)
│   │   ├─ inventory.rs    界面用的清单树
│   │   └─ remote.rs       云端目录树、单文件传输、差异对照
│   ├─ store/              SQLite 缓存与文件存储
│   ├─ paths.rs            资源定位
│   └─ process.rs          子进程启动(禁止弹控制台窗口)
├─ src-cli/                命令行
├─ src-tauri/              桌面壳(Tauri 2)
└─ ui/                     前端(原生 JS,无框架)
```

约 20600 行 Rust + 3000 行前端,**525 个测试**。

---

## 数据存到哪

默认 `%APPDATA%\recording-summary\data\`,可以改到别的盘:

```powershell
rs config data-dir show
rs config data-dir move D:\RecordingSummary
```

位置记在 `%APPDATA%\recording-summary\location.txt`(一行绝对路径)。用文件而不是注册表或环境变量 —— **双击 exe 时设不了环境变量**,而这是最常见的启动方式。

体积基本全在录音上:1 小时 wav 约 115 MB、m4a 约 28 MB;所有文本产物加起来只有几百 KB。

---

## 开发

```powershell
.\scripts\cargo.ps1 build            # debug
.\scripts\cargo.ps1 build --release  # release
.\scripts\cargo.ps1 test             # 525 个测试
```

### 编译 whisper.cpp

```powershell
# 需要先加载 MSVC 环境
cmake -B build-cpu -DCMAKE_BUILD_TYPE=Release
cmake --build build-cpu --config Release
# 产物在 build-cpu\bin\Release\
```

CUDA 版有三个卡点(详见技术方案 §16.5):

1. `nvidia-smi` 显示的 "CUDA Version" 是**驱动支持的上限**,不是已安装版本。装高于它的工具链会"装得上但跑不起来"。
2. CMake 需要 `-DCudaToolkitDir="<root>\\"` —— **结尾反斜杠是必需的**。
3. 除了 `whisper-cli.exe`,还要拷 `whisper.dll`、`ggml*.dll` 以及运行时库(`cudart64_*.dll`、`cublas64_*.dll`)。

<details>
<summary><b>Windows 开发环境的几个坑</b></summary>

**不要用 PowerShell 5.1 读写本仓库的中文文件。** `Get-Content` / `Set-Content` 默认按系统 ANSI 代码页(GBK)解码,会把 UTF-8 的中文永久写成 `?`。用编辑器,或显式指定 `[System.IO.File]::ReadAllText($p, [Text.Encoding]::UTF8)`。

**`.ps1` 文件必须带 UTF-8 BOM。** 否则 PowerShell 5.1 会按 GBK 解码,里面的中文变成乱码,甚至报出莫名其妙的语法错误("string is missing the terminator")。用 VS Code 时注意右下角的编码显示,要选 "UTF-8 with BOM"。

**PowerShell 的 `cd` 跨盘时只切"记忆中的目录",不切当前盘符。** 之后用相对路径仍会解析到旧盘。跨盘要用 `Set-Location -LiteralPath`,或者干脆用绝对路径。

**不要批量 kill 系统进程。** 一次 `Get-Process conhost | Stop-Process` 会让所有命令行会话失效,包括你自己的构建环境。

**`git` 的 `core.quotePath`** 默认把中文文件名显示成八进制转义。`git config core.quotepath false` 可读性更好。

</details>

---

## 已知限制

- **只验证过 Windows。** 后端选择、路径处理、进程创建都按 Windows 优先写的。
- **双轨录音未实现。** 即"麦克风录本地、系统声卡录远端"自动分轨 —— 这是让说话人区分接近零错误的最优方案,但需要真实会议场景才能验证。
- **说话人区分没在真实多人录音上验证过。** 开发时只有 TTS 合成语音,单一音色会被切成多个虚假说话人。真实录音下的准确率未知。
- **Vulkan 后端未编译。** 代码路径存在,但 `binaries/vulkan/` 是空的。
- **说话人区分的耗时未充分评估。** 2 小时 18 分的录音在 CUDA 上转写约 6 分钟,而说话人区分要跑更久(它要处理全部音频的声纹嵌入)。超长录音建议先跳过它。
- **CrispASR 引擎比 whisper 慢约 6 倍。** 2 小时录音 whisper 约 10 分钟、FireRedASR2 约 50 分钟(它的解码器跑在 CPU 上)。
- **CrispASR 用不了 `--vad` 和 `--chunk-seconds 0`。** v0.8.32 上这两个都会 2 秒跑完、零输出,只能用默认的 30 秒切片。
- **CrispASR 有已知的时间戳顺序问题**(它自己的 issue #356)。会报
  `transcript is not in time order after slice merge`,影响 SRT 的分段。
- **CrispASR 的中英混说未充分验证。** 它官方声明支持 code-switching,但我们的实测里
  中英混说那一项是它唯一没赢过 whisper 的地方。

### 已经验证过的

- **真实 WebDAV 服务器**(Seafile 系):连接、目录创建、上传、下载、删除、单文件同步都跑通过。
  过程中发现并修掉了三个只有真实服务器才能暴露的问题 —— 认证头缺失、路径未做百分号编码(中文路径 409)、
  以及云端目录树的两处组装错误。
- **2 小时 18 分的真实课堂录音**:转写 2358 段、详细总结 18 KB、思维导图正常。
- **两个引擎的对照实测**(同一段 5 分钟真实录音,逐项核对):见上面「几个不显然的技术选择」里的对比表。
- **转写纠错**:用 5 个预埋错误的合成音频 + 一段繁体转写验证,5 处全部纠正、繁体转简体。
- **437 → 525 个测试**,覆盖选择规则、清单合并、状态判定、路径编码、树组装、路径安全、删除语义、纠错安全、引擎选择。

---

## 许可证

MIT,见 [`LICENSE`](LICENSE)。
