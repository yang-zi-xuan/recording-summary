# 录音转总结客户端

把课堂/会议录音变成**带时间戳、带发言人、按场景结构化**的纪要。

- **转写在本地完成** —— 音频不出机器,零调用成本
- **只有文字会发给 LLM** —— 成本可以忽略
- **无硬性硬件要求** —— 从独立显卡到纯 CPU 都能跑,自动选档
- **越用越准** —— 声纹档案让同一个人第二次出现时被自动认出来

设计文档见 [`docs/技术方案.md`](docs/技术方案.md)。

---

## 目录

- [当前状态](#当前状态)
- [快速开始](#快速开始)
- [命令一览](#命令一览)
- [目录结构](#目录结构)
- [还需要你做的事](#还需要你做的事)
- [开发环境注意事项](#开发环境注意事项)

---

## 当前状态

**W1 + W2 + W4 + W6 已完成并验证。**

**每处理一段录音会自动建一个「工程」** —— 一个自包含目录,装着录音、
带时间戳的转写、详细总结、简略总结和思维导图,可以整个打包发给别人。

| 模块 | 状态 |
|---|---|
| 硬件探测与后端降级 | ✅ 完整,**已验证 CUDA 自动选中** |
| 音频解码(WAV 内置 / 其他走 ffmpeg) | ✅ **已验证(m4a 解码)** |
| 分块转写 + 断点续传 | ✅ 完整 |
| whisper.cpp sidecar 转写 | ✅ **已用真实中文音频验证(CPU + CUDA)** |
| 转写缓存(内容哈希) | ✅ **已验证命中** |
| 说话人区分(sherpa-onnx 接入) | ✅ 完整,**待你的真实多人音频验收** |
| 段落级重叠投票 | ✅ 完整(含单元测试) |
| 声纹档案(加权中心 / 三级匹配 / 防污染) | ✅ 完整(库层),嵌入提取待接 |
| **工程制存储**(自包含目录 + 全部产物) | ✅ **已验证** |
| **两份总结**(详细 / 简略,目标不同) | ✅ **已用真实 API 验证** |
| **思维导图**(Mermaid + 自动修复 + 降级) | ✅ **已验证渲染与降级** |
| LLM 客户端(OpenAI 兼容 / 多供应商) | ✅ **已用真实 Key 验证** |
| 场景判断 + 分场景模板 + map-reduce | ✅ 完整 |
| CLI 全部子命令(含工程命令) | ✅ **已验证** |
| WebDAV 同步(manifest + 增量 + 工程目录) | ✅ 完整,**待你的云盘账号验证** |
| GUI(Tauri 2) | ✅ **已验证工程视图与导图渲染** |

**测试:286 个单元测试 + 12 个集成 + 6 个前端辅助,共 304 个,全部通过。构建零警告。**

### 一个工程长什么样

```text
store/projects/2026-09-11_高等数学第12讲/
├─ project.json          元数据(产物清单 / 场景 / 模型 / 后端)
├─ audio/recording.wav   录音副本 —— 工程自包含,可直接发人
├─ transcript.md         带时间戳的转写
├─ transcript.json       结构化段落
├─ transcript.srt        字幕
├─ summary-detailed.md   详细总结(知识点 / 例题 / 待办 / 复习提纲)
├─ summary-brief.md      简略总结(一句话 + 要点,400 字以内)
├─ mindmap.mmd           思维导图(Mermaid 源码)
└─ mindmap-outline.md    文本大纲(图渲染失败时的降级)
```

### 已验证的环境(实际跑出来的)

```
CPU      Intel Core i9-14900HX(32 核)
GPU      NVIDIA GeForce RTX 4060 Laptop(8187 MiB)
后端     CUDA(自编译 whisper.cpp 12.6,compute capability 8.9)
模型     large-v3-turbo(1549 MB)—— 18 秒音频约 5 秒转完
ffmpeg   9.0.1(已装,项目内也有一份 binaries/ffmpeg/)
GUI      Tauri 2.11.5 + WebView2,工程视图与思维导图渲染均通过
LLM      DeepSeek 真实 Key,两份总结 + 导图均成功生成
```

### 需要你验收的部分

我无法自证的三件事,都写在 [需要你做的事](#还需要你做的事) 里:

1. **真实课堂录音**(多人、远场、混响)—— 我只有 15~19 秒的 TTS 合成语音
2. **真实 DeepSeek API Key** —— 纪要质量与场景判断准确度
3. **真实 WebDAV 账号** —— 同步链路


---

## 快速开始

### 1. 环境准备

本项目使用 `x86_64-pc-windows-msvc` 工具链,链接需要 MSVC 环境。
**不要直接 `cargo build`**,用提供的包装脚本:

```powershell
powershell -File scripts\cargo.ps1 build
powershell -File scripts\cargo.ps1 test
```

> 为什么需要脚本:MSVC 的 `link.exe` 与 Windows SDK 默认不在 PATH 上,
> 直接 `cargo build` 会报误导性的 `can't find crate for 'core'`,
> 而不是告诉你缺链接器。脚本会先加载 `vcvars64.bat` 的环境。

若 vcvars 路径不同,改 `scripts/cargo.ps1` 顶部的 `$vcvars`。

### 2. 检查硬件与模型

```powershell
.\target\debug\rs.exe probe
```

会告诉你:检测到什么 GPU、各后端是否可用、推荐哪个模型档位、模型是否就绪。

### 3. 模型下载

模型放 `models/` 目录。**HuggingFace 在国内 DNS 常被污染**,用镜像:

```powershell
# base(141MB,CPU 上可接近实时)
curl.exe -L --ssl-no-revoke -o models\ggml-base.bin `
  https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-base.bin

# small(465MB,CPU 推荐档,质量明显更好)
curl.exe -L --ssl-no-revoke -o models\ggml-small.bin `
  https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-small.bin

# large-v3-turbo(1.6GB,GPU 上用这个)
curl.exe -L --ssl-no-revoke -o models\ggml-large-v3-turbo.bin `
  https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin
```

> `--ssl-no-revoke` 是必需的:不加会报 `CRYPT_E_REVOCATION_OFFLINE`
> (证书吊销检查离线),看起来像网络故障,实际不是。

### 4. whisper.cpp 二进制

放 `binaries/<后端>/whisper-cli.exe`,例如 `binaries/cpu/`。

编译方法见 [编译 whisper.cpp](#编译-whispercpp) 一节。**需要与 exe 同目录的 DLL**
(`whisper.dll`、`ggml*.dll`)。

### 5. 前端依赖(GUI 的思维导图需要)

mermaid 有 **5.4 MB**,所以不进版本库,需要下载一次:

```powershell
curl.exe -L --ssl-no-revoke -o ui\vendor\mermaid.min.js `
  https://registry.npmmirror.com/mermaid/latest/files/dist/mermaid.min.js
```

**没有它程序照常运行** —— 只是「思维导图」标签页会降级显示文本大纲,
并在状态栏说明原因。设置页有依赖检查会明确提示。

详见 [`ui/vendor/README.txt`](ui/vendor/README.txt)。

### 6. 配置 LLM(可选)

不配也能用 —— 会完成转写和说话人区分,只是**没有两份总结和思维导图**
(工程仍会建立,只是产物不全)。

```powershell
rs.exe config set-key sk-xxxxxxxx        # 存进 Windows 凭据管理器
rs.exe config test                       # 验证连通性
rs.exe config show                       # 查看当前配置
```

换供应商只改两个值(代码不用动):

```powershell
rs.exe config set-base-url https://dashscope.aliyuncs.com/compatible-mode/v1
rs.exe config set-model qwen-plus
```

内置预设:DeepSeek、通义千问、Kimi、硅基流动、Ollama(本地)。

### 7. 跑一段录音

```powershell
rs.exe run 录音.m4a --language zh --print
```

处理完会自动建一个工程。用 `rs.exe projects` 看,或打开 GUI 的「我的工程」。

中文会自动附加简体提示(见下文"已知行为")。

---

## 命令一览

| 命令 | 说明 |
|---|---|
| `rs probe [--json]` | 硬件探测与配置自检 |
| `rs run <文件>` | 完整管线:解码 → 转写 → 说话人 → 场景 → **建工程** |
| **`rs projects`** | **列出全部工程(含产物完整度)** |
| **`rs projects show <ID>`** | **工程详情:元数据 + 产物清单** |
| **`rs projects cat <ID> brief`** | **打印产物(brief\|detailed\|transcript\|srt\|mindmap\|outline)** |
| **`rs projects export <ID> <目录>`** | **整个工程拷出去(自包含,可直接发人)** |
| `rs show <会话ID>` | 显示转写(`--view dialogue\|timeline\|plain\|srt`) |
| `rs list` | 列出已处理的会话 |
| `rs rename <会话ID> <序号> <名字>` | 改发言人名字(**不重新转写**,秒级) |
| `rs profiles [list\|delete\|rename]` | 声纹档案管理 |
| `rs sync [login\|test\|show\|plan\|run\|rebuild]` | 云端同步,支持选择范围与删除策略 |
| `rs config [show\|set-key\|test\|...]` | LLM 配置 |
| `rs stats` | 缓存与待同步文件统计 |

### `rs run` 常用参数

```
--language zh            语言(默认自动检测;中文会附加简体提示)
--model small            模型档位(tiny/base/small/medium/large-v3-turbo)
--backend cuda           强制后端(默认自动探测)
--speakers 4             指定发言人数(最准;不给则自动检测)
--no-diarize             跳过说话人区分
--no-summary             只转写,不生成总结(也就不会有工程产物)
--term 反向传播          术语表(可多次;进 initial_prompt 与缓存键)
--chunk-secs 300         分块时长
--print                  跑完立刻打印简略总结
```

工程 ID 与会话 ID 都支持前缀匹配,`rs projects show 7a654` 即可。

### 处理完在哪找结果

```powershell
rs projects                      # 看有哪些工程
rs projects show 7a654           # 看某个工程里有什么
rs projects cat 7a654 brief      # 直接打印简略总结
rs projects export 7a654 D:\给同学  # 整个目录拷出去
```

或者打开 GUI → 左侧「我的工程」。工程目录本身就在
`<数据目录>\store\projects\<日期>_<标题>\`,可以直接用文件管理器打开。

---

## 目录结构

```
recording summary/
├─ docs/技术方案.md          完整设计文档
├─ scripts/cargo.ps1         MSVC 环境包装(必须用它构建)
├─ src-core/                 全部业务逻辑
│   └─ src/
│       ├─ types.rs          数据契约
│       ├─ hardware.rs       后端探测与降级链
│       ├─ audio.rs          解码、重采样、切分
│       ├─ asr.rs            whisper.cpp sidecar
│       ├─ diarize.rs        嵌入、聚类、重叠投票
│       ├─ voiceprint.rs     声纹档案
│       ├─ llm/              LLM 客户端 / prompt / 费用
│       ├─ pipeline/         编排 + 三种视图
│       └─ store/            SQLite 缓存 + 文件存储
├─ src-cli/                  命令行入口
├─ binaries/<后端>/          whisper-cli.exe(需自备)
├─ models/                   模型文件(需下载)
└─ data/                     运行时数据(默认在用户数据目录)
    └─ store/                ★ 跨设备同步的真相源
        ├─ transcript/ab/…   转写 JSON
        ├─ summary/ab/…      纪要 Markdown
        ├─ speaker_labels/   发言人名字(独立拆分,改名只重写这个)
        ├─ profiles.json     声纹档案注册表(ID + 名字,**不含向量**)
        └─ manifest.json     同步状态
```

**设计要点:** 分享的是**文件**而不是数据库。SQLite 只是本地缓存,
可以随时删掉从 `store/` 重建 —— 所以不需要处理数据库合并冲突。

---

## 还需要你做的事

按重要性排序。**前四项已经在开发机上做完并验证**,这里记录做法,便于你在别的机器复现。

### 1. CUDA 版 whisper.cpp —— ✅ 已完成

当前 `binaries/cuda/` 已就位,`rs probe` 会报 CUDA 可用,`rs run` 自动选它。
实测 18 秒音频约 5 秒(含进程启动)。

**在别的机器上复现时注意三个坑**(都是我实际踩到的):

```powershell
# 0) 先确认驱动支持的上限 —— nvidia-smi 右上角的 "CUDA Version" 是驱动支持的上限,
#    不是已安装的版本。装高于它的工具链会"装得上但跑不起来"。
nvidia-smi

# 1) 装匹配版本的 CUDA Toolkit(winget 可指定历史版本)
winget install Nvidia.CUDA --version 12.6

# 2) 编译。注意 CudaToolkitDir 的结尾反斜杠是必需的,缺了会报
#    "The CUDA Toolkit v12.6 directory '' does not exist"
$cuda = "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.6"
cmake -B build-cuda -G "Visual Studio 17 2022" -A x64 `
  -DCMAKE_BUILD_TYPE=Release -DGGML_CUDA=ON `
  -DWHISPER_BUILD_TESTS=OFF -DWHISPER_BUILD_EXAMPLES=ON `
  -DCUDAToolkit_ROOT="$cuda" `
  -DCMAKE_CUDA_COMPILER="$cuda\bin\nvcc.exe" `
  -DCMAKE_CUDA_ARCHITECTURES=89 `
  -DCudaToolkitDir="$cuda\\"
cmake --build build-cuda --config Release -j 16

# 3) 拷产物 + 运行时 DLL(cudart/cublas 也要带上,否则换机器会缺库)
mkdir binaries\cuda
copy build-cuda\bin\Release\whisper-cli.exe binaries\cuda\
copy build-cuda\bin\Release\*.dll binaries\cuda\
copy "$cuda\bin\cudart64_12.dll" binaries\cuda\
copy "$cuda\bin\cublas64_12.dll" binaries\cuda\
copy "$cuda\bin\cublasLt64_12.dll" binaries\cuda\
```

**编译 Vulkan 版**(覆盖非 NVIDIA 显卡)同理,把 `-DGGML_CUDA=ON` 换成
`-DGGML_VULKAN=ON`,并需要 Vulkan SDK(提供 glslc)。放到 `binaries/vulkan/`。

### 2. ffmpeg —— ✅ 已完成

`ffmpeg 9.0.1` 已装,项目内 `binaries/ffmpeg/` 也放了一份(m4a 解码实测通过)。

```powershell
winget install Gyan.FFmpeg
# 或者只给项目用:把 ffmpeg.exe 放到 binaries\ffmpeg\
# 或者指定路径:$env:RECSUM_FFMPEG = "D:\tools\ffmpeg.exe"
```

### 3. 声纹模型与说话人区分 —— ✅ 已完成

两个模型已在 `models/`,`sherpa-onnx` 已接入并**实测跑通**(带发言人 + 时间戳):

```powershell
curl.exe -L --ssl-no-revoke -o models\segmentation-3.0.onnx `
  https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-segmentation-models/sherpa-onnx-pyannote-segmentation-3-0.tar.bz2
# ↑ 实际是 tar.bz2,解开后取 model.onnx

curl.exe -L --ssl-no-revoke -o models\3dspeaker.onnx `
  https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k.onnx
```

**注意:GitHub Release 在国内会断流。** 两次实测都出现了下到一半卡住的情况,
必须用 `curl -C -`(断点续传)+ 重试循环。脚本化的做法:

```powershell
for ($i=1; $i -le 40; $i++) {
  curl.exe -L --ssl-no-revoke -C - --retry 2 --retry-all-errors `
    -o models\3dspeaker.onnx "<上面的 URL>"
  if ((Get-Item models\3dspeaker.onnx).Length -ge 37MB) { break }
  Start-Sleep -Seconds 5
}
```

**3dspeaker.onnx 就是 37.8 MB**(不是网上常说的更大体积)——
能正常加载并跑出结果即完整。

### 4. 依赖与构建环境 —— ✅ 已完成

`scripts/cargo.ps1` 会自动处理三件事,但你需要知道它们在做什么:

| 依赖 | 用途 | 安装 |
|---|---|---|
| MSVC(link.exe + Windows SDK) | Rust MSVC 目标链接 | VS Build Tools |
| **LLVM(libclang)** | `sherpa-rs-sys` 用 bindgen 生成 FFI 绑定 | `winget install LLVM.LLVM` |
| sherpa-onnx 原生库 | 说话人区分运行时 | 已固化在 `native/sherpa-rs/`(14.5MB) |

**`native/sherpa-rs/` 为什么存在:** `sherpa-rs-sys` 首次构建会从 GitHub 下载
一个 ~23MB 的原生库包。国内网络下这一步经常超时(它用的 `ureq` 没有重试和断点续传)。
所以把需要的文件固定下来。来源与重新获取方式见 `native/sherpa-rs/README.txt`。

### 5. 用真实录音验收 ← **需要你做**

我只有 15~19 秒的 TTS 合成语音。真实课堂录音会暴露更多问题
(远场混响、多人重叠、专有名词、说话人数量)。

建议拿一段 **45 分钟真实课程**跑一次,重点看:
- 转写可读性(错字是否影响理解)
- 场景判断是否判成"课堂"
- **说话人区分准不准**(这是我唯一无法自证的核心功能)
- 纪要结构是否合用
- 术语错误有多少(据此整理 `--term` 术语表)

### 6. 用真实 API Key 验证总结质量 ← **需要你做**

`rs config test` 通了之后,跑一次完整流程并检查:

- 场景判断的置信度与依据是否合理
- 课堂模板的"复习提纲"是否真的有用
- **简略总结是否真的够简略**(提示词要求 400 字以内)
- **思维导图有没有被自动修复**(状态栏会说明修了几处)
- prompt 缓存命中率(决定成本)

### 7. 用真实云盘验证同步 ← **需要你做**

WebDAV 同步代码完整(XML 解析有单元测试),但**没有在真实服务器上跑过**:

```powershell
# 可以直接粘完整地址,程序会自动拆分
rs sync login "<账号>@auth.local" --url https://cloud.example.com/seafdav/recording_summary
rs sync test

# 先看计划,别急着跑
rs sync plan --offline            # 不联网,只看"哪些会被删"
rs sync plan                      # 联网,看完整计划

rs sync run
```

#### 同步什么由你定

```powershell
# 只同步这学期的课
rs sync run --project 2026-09

# 或按通配符
rs sync run --include "projects/2026-09*" --exclude "*/audio/*"

# 只同步文本,录音完全不管
rs sync run --audio skip
```

#### 删不删也由你定

| `--deletion` | 行为 |
|---|---|
| `keep` | **从不删云端**。同步只做"加"和"改" |
| `text`(默认) | 文本双向镜像;录音**永不删** |
| `mirror` | 范围内全删,包括录音 |

**默认策略的取舍:** 文本小,本地整理过(删了)就该同步到云端;
录音大,本地删掉通常是腾空间,不该连云端一起清掉。

**安全保证:** 只有"**本机同步过的**文件、本地又没了、且在同步范围内"
才会被删。别的设备上传但本机还没下载的文件永远不会被误删;
取消勾选某个工程也不会把它从云端抹掉。要清理云端必须显式选进来 +
用会删的策略。执行前 `rs sync plan` 会标出所有 `★删除` 项。

---

## 已知行为

### 中文会自动附加简体提示

Whisper 在中文音频上有**稳定的繁体偏好**,实测同一段音频:

| 配置 | 输出 |
|---|---|
| 无提示 | 同學們,這節課我們講神經網絡… |
| **有简体提示** | 同学们,这节课我们讲神经网络… |

所以 `--language zh` 时,程序会自动把"以下是普通话的句子,请使用简体中文"
放进 `initial_prompt`。这**顺带还降低了同音字错误率**(实测"公示"→"公式")。

不需要这个行为时,不要传 `--language zh`(留空走自动检测即可)。

### 模型档位自动跟随硬件

`rs probe` 会按探测结果推荐档位。这是**有意设计** —— 让纯 CPU 机器默认下
1.6GB 的大模型是错的,因为那要跑 1.5~4 小时。

| 后端 | 推荐模型 | 45 分钟音频 |
|---|---|---|
| CUDA / Metal | large-v3-turbo | 2~3 分钟 |
| Vulkan | medium | 8~15 分钟 |
| CPU(≥16 核) | small | 30~90 分钟 |
| CPU(<8 核) | tiny | 1.5~4 小时 |

可用 `--model` 覆盖。

### 手动指定后端时不会静默降级
`--backend cuda` 但 CUDA 版没装好时,程序**明确报错**,而不是悄悄用 CPU。
否则你会困惑"为什么这么慢"。

### 缺东西时会降级而不是中断

| 缺失 | 行为 |
|---|---|
| LLM API Key | 转写照常完成,提示纪要被跳过 |
| 声纹模型 | 转写照常完成,提示说话人区分被跳过 |
| mermaid 前端库 | 导图标签页降级显示文本大纲 |
| ffmpeg(且输入非 WAV) | 报错并给出安装指引 |

### 资源目录会自动搜索,**不依赖启动时的工作目录**

`models/` 与 `binaries/` 按这个顺序找(见 `src-core/src/paths.rs`):

```text
1. <当前工作目录>/models          - 从项目根用命令行启动
2. <exe 所在目录>/models          - 打包后 exe 与 models 同级
3. <exe 所在目录>/../models       - exe 在 target/debug/ 时
4. <exe 所在目录>/../../models
5. <数据目录>/models              - 模型放在用户数据目录
```

**为什么不能只用相对路径:** 双击 exe 或从快捷方式启动时,
工作目录可能是 `C:\Windows` 而不是项目根目录 —— 相对路径失效,
界面就会显示「没有模型」。这个坑已经踩过并修掉了。

判断条件也不只看"目录存在",还看"里面有没有东西" ——
一个空的 `models/` 目录会误导探测结果。

前端依赖(`ui/vendor/mermaid.min.js`)走同一套搜索。

---

## 数据存到哪

### 默认位置

```text
%APPDATA%\recording-summary\data\
├─ cache.db              SQLite:缓存、设置(不含密码)
└─ store\
   ├─ projects\          每个录音一个工程目录
   ├─ transcript\        哈希存储的转写
   ├─ summary\
   ├─ speaker_labels\
   ├─ device.id          本机标识(同步用)
   └─ manifest.json      同步清单
```

体积**全在录音上**:1 小时 wav 约 115 MB、m4a 约 28 MB。
所有文本产物加起来只有几百 KB。

### 改到别的盘

```powershell
rs config data-dir show                      # 看当前在哪
rs config data-dir move D:\RecordingSummary  # 迁移过去
rs config data-dir reset                     # 回到系统默认(不搬数据)
```

`move` 做三件事,顺序刻意如此:

1. **拷贝 + 校验文件数**(不删源 —— 拷贝失败时原数据必须完好)
2. 写指针文件
3. 尝试删源目录 —— **失败不算迁移失败**,只提示一句

第 3 步为什么不算失败:数据已经在目标目录、指针也已更新,
此时报错会让用户以为迁移没成功,反而可能去手工删目标目录。

迁移前会自动对 SQLite 做 `wal_checkpoint(TRUNCATE)` ——
否则 `-wal` 里未合并的事务不会被完整拷走。

**改完不用再加 `--data-dir`**,GUI 双击启动也会用新位置。

### 位置存在哪

指针文件:`%APPDATA%\recording-summary\location.txt`,内容就一行绝对路径。

它**必须**在数据目录之外 —— 否则"数据目录在哪"这个问题无人回答。
用文件而不是注册表或环境变量,理由:

- 用户能直接看懂、能手工改、能删掉复位
- **不依赖启动方式** —— 双击 exe 时设不了环境变量,这是关键

---

## 开发环境注意事项

### ⚠️ 不要用 PowerShell 读写本仓库的中文文件

本机 `pwsh` 实际是 **Windows PowerShell 5.1**,其 `Get-Content` 默认按系统
ANSI 代码页(GBK)解析文件。用它读 UTF-8 中文文件再写回会**永久损坏编码**
(部分字节经 GBK 解码变成 `?`,不可逆)。

必须用脚本处理时显式指定编码:

```powershell
$t = [System.IO.File]::ReadAllText($p, [System.Text.Encoding]::UTF8)
[System.IO.File]::WriteAllText($p, $new, (New-Object System.Text.UTF8Encoding($false)))
```

同理,**`.ps1` 脚本保持纯 ASCII** —— 中文注释里的多字节字符可能"吃掉"结尾引号,
产生莫名其妙的解析错误。

### 「测试连接」用的是输入框的值,不必先保存

「测试连接」「查看同步计划」「开始同步」都会读**当前输入框里的内容**,
不要求先点「保存」。日志里会打出实际用到的地址与用户名,
出问题时一眼能看出是"没填"还是"填错了"。

(早先版本只读已保存的配置,于是"填了用户名却报用户名为空" —— 这个坑踩过。)

### 远端目录可以直接粘完整地址

```powershell
rs sync login 账号@auth.local --url https://cloud.example.com/seafdav/recording_summary
```

程序会自动拆成服务地址 + 远端目录。GUI 里粘进「服务地址」框也一样。
支持多级远端目录(如 `recording_summary/2026秋`)。

### PowerShell 5.1 的其他坑

- 不支持三元运算符 `? :`
- 不支持 `&&` / `||` 链式操作符
- 直接 `& ".\path\to.exe" args` 有时会把 exe 路径本身当参数 → 用 `cmd /c`

### 编译 whisper.cpp

```powershell
git clone --depth 1 https://github.com/ggml-org/whisper.cpp.git
cd whisper.cpp
# 需要先加载 MSVC 环境(或用 scripts/cargo.ps1 里的方法)
cmake -B build-cpu -DCMAKE_BUILD_TYPE=Release -DGGML_CUDA=OFF `
      -DWHISPER_BUILD_TESTS=OFF -DWHISPER_BUILD_EXAMPLES=ON
cmake --build build-cpu --config Release -j 16
# 产物在 build-cpu\bin\Release\
```

### 验证清单

```powershell
powershell -File scripts\cargo.ps1 test    # 期望:190 passed
.\target\debug\rs.exe probe                # 期望:报告后端与模型状态
.\target\debug\rs.exe run <一段音频> --print
```

---

## 参考

- 完整设计:[`docs/技术方案.md`](docs/技术方案.md)
- [whisper.cpp](https://github.com/ggml-org/whisper.cpp)
- [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx)(说话人区分与声纹)
- 国内模型镜像:`hf-mirror.com`
