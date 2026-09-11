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
| **转写在本机** | whisper.cpp sidecar。音频不出机器,零调用成本,可离线 |
| **自动选后端** | 按 `CUDA → Vulkan → CPU` 顺序探测,不预设你有什么硬件 |
| **说话人区分** | sherpa-onnx 离线聚类 + 段落级重叠投票,可手动指定人数 |
| **声纹档案** | 同一个人第二次出现时自动认出;三级匹配(确认/待定/新建)让错误可见 |
| **中英混合** | 语码转换天然支持;简体提示无条件注入(理由见下) |
| **场景自适应** | 自动判断课堂/会议/访谈,套用不同的总结模板 |
| **长音频** | 分块转写 + 断点续传;超出上下文时走 map-reduce |
| **工程制** | 每次处理产生一个自包含目录,可打包、可同步、可阅读 |
| **选择性同步** | 方向、范围、音频、删除策略四个独立维度,可精确到单个文件 |
| **后端可替换** | 切换后端 = 换一个 exe 路径,不需要重新编译主程序 |

---

## 几个不显然的技术选择

理由都来自实测,详细论证见技术方案。

**whisper.cpp 走 sidecar 子进程,不走 FFI。** 换后端只需换一个 exe 路径;子进程崩溃带不走 GUI;不同后端的构建产物互不干扰。代价是进程启动开销,对分钟级音频可以忽略。

**说话人区分用 sherpa-onnx,不用 pyannote。** 后者要带一个 Python 运行时,而这是要分发给普通用户的桌面程序。

**段落级重叠投票,不用词级时间戳。** whisper.cpp 的 `--max-len 1` 在 CJK 上工作得很差。改用分块重叠 + 投票,精度够用且没有 CJK 问题。

**简体提示无条件注入 `initial_prompt`,不按语言判断。** Whisper 在中文音频上有稳定的繁体偏好(实测"同學們""神經網絡"),而**语言检测不等于内容语言** —— 一门英文术语密集的课会被判成 `en`,中文部分照样要出简体。按 `language.starts_with("zh")` 判断会让"自动检测"这个最适合混合内容的选项拿不到保护。

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

# large-v3-turbo(1549 MB,GPU 上用这个)
curl.exe -L -o models/ggml-large-v3-turbo.bin "$base/ggml-large-v3-turbo.bin"
```

说话人区分还需要两个 ONNX 模型(共 43 MB),见[技术方案 §7](docs/技术方案.md)。

### 3. whisper.cpp 二进制

放到 `binaries/<后端>/whisper-cli.exe`,例如 `binaries/cpu/`。**需要与 exe 同目录的 DLL**(`whisper.dll`、`ggml*.dll`)。

编译方法见[编译 whisper.cpp](#编译-whispercpp)。

### 4. 前端依赖

思维导图用 mermaid(5.4 MB),不进版本库:

```powershell
curl.exe -L --ssl-no-revoke -o ui\vendor\mermaid.min.js `
  https://registry.npmmirror.com/mermaid/latest/files/dist/mermaid.min.js
```

**没有它程序照常运行** —— 只是思维导图标签页会降级显示文本大纲,并在状态栏说明原因。

### 5. 配置 LLM

不配也能用:会完成转写和说话人区分,只是没有总结和思维导图。

```powershell
.\target\debug\rs.exe config set-key sk-xxxxxxxx   # 存进 Windows 凭据管理器
.\target\debug\rs.exe config test                  # 验证连通性
```

API Key 与 WebDAV 密码都存在**系统凭据管理器**,不写进任何配置文件。

### 6. 跑一段录音

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
| `rs show <会话ID>` | 显示转写(`--view dialogue`/`timeline`/`plain`/`srt`) |
| `rs rename <会话ID> <序号> <名字>` | 改发言人名字(不重新转写,秒级) |
| `rs profiles` | 声纹档案管理 |
| `rs sync [plan\|run]` | 云端同步 |
| `rs config data-dir` | 查看/迁移数据目录 |
| `rs stats` | 缓存统计 |

### `rs run` 常用参数

```text
--language zh            语言(默认自动检测;简体提示始终注入)
--model small            模型档位(tiny/base/small/medium/large-v3-turbo)
--backend cuda           强制后端(默认自动探测;失败不静默降级)
--speakers 4             指定发言人数(最准;不给则自动检测)
--no-diarize             跳过说话人区分
--no-summary             只转写,不生成总结
--term 反向传播          术语表(进 initial_prompt 与缓存键)
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

---

## 项目结构

```
recording-summary/
├─ docs/技术方案.md        完整设计文档(取舍、实测数据、踩坑记录)
├─ scripts/cargo.ps1       MSVC 环境包装(推荐用它构建)
├─ src-core/               全部业务逻辑
│   ├─ asr.rs              whisper.cpp sidecar
│   ├─ diarize.rs          说话人聚类
│   ├─ sherpa.rs           sherpa-onnx 接入
│   ├─ hardware.rs         后端探测与降级链
│   ├─ audio.rs            解码、重采样、分块
│   ├─ project.rs          工程制存储
│   ├─ llm/                LLM 客户端与提示词
│   ├─ sync/               同步(选择、清单、状态、墓碑)
│   ├─ store/              SQLite 缓存与文件存储
│   ├─ paths.rs            资源定位
│   └─ process.rs          子进程启动(禁止弹控制台窗口)
├─ src-cli/                命令行
├─ src-tauri/              桌面壳(Tauri 2)
└─ ui/                     前端(原生 JS,无框架)
```

约 17000 行 Rust + 2200 行前端,**437 个测试**。

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
.\scripts\cargo.ps1 test             # 437 个测试
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
- **没有在真实多说话人录音上验证过。** 开发时只有 TTS 合成语音,单一音色会被切成多个虚假说话人。真实录音下的准确率未知。
- **没有在真实 WebDAV 服务器上端到端验证过。** 同步逻辑有单元测试,但首次连接真实网盘仍可能遇到服务端差异。
- **Vulkan 后端未编译。** 代码路径存在,但 `binaries/vulkan/` 是空的。

---

## 许可证

MIT,见 [`LICENSE`](LICENSE)。
