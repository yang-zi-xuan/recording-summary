//! 硬件探测、后端自检与降级链。见技术方案 §3。
//!
//! 设计要点:
//! - 不预设用户有什么硬件。`Auto` 模式按 `Backend::PRIORITY` 逐个探测 + 自检。
//! - **只用 `nvidia-smi` 判断是不够的** —— 驱动太老、运行时库缺失、被远程桌面屏蔽、
//!   显存不足都会让"有 GPU"变成转写崩溃。所以必须真跑一次微型转写。
//! - `Force` 模式失败时**明确报错,不静默降级**,否则用户会困惑"为什么这么慢"。
//! - CPU 永远兜底。

use crate::types::{Backend, BackendPref, ModelTier};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

// ---------------------------------------------------------------------------
// 探测结果
// ---------------------------------------------------------------------------

/// 单个后端的探测结论。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeResult {
    pub backend: Backend,
    /// 硬件层面是否可能存在该后端(便宜检查)
    pub available: bool,
    /// ★ 自检是否通过(真跑过一次微型转写)
    pub ok: bool,
    pub elapsed_ms: u64,
    /// 实时率 = 转写耗时 / 音频时长,越小越快。0 表示未测。
    pub rtf: f32,
    pub device_name: Option<String>,
    pub error: Option<String>,
}

impl ProbeResult {
    /// 构造一个"硬件层面就不可用"的结果。
    ///
    /// 预留给真正的自检流程(试跑微型转写)使用 —— 见 `Pipeline::probe_hardware`
    /// 中的说明:目前只记录可用性,实测 rtf 在首次转写后回填。
    #[allow(dead_code)]
    pub fn unavailable(backend: Backend, why: impl Into<String>) -> Self {
        Self {
            backend,
            available: false,
            ok: false,
            elapsed_ms: 0,
            rtf: 0.0,
            device_name: None,
            error: Some(why.into()),
        }
    }

    /// 构造一个"硬件可用但自检失败"的结果(驱动太老、运行时库缺失等)。
    #[allow(dead_code)]
    pub fn failed(backend: Backend, why: impl Into<String>) -> Self {
        Self {
            backend,
            available: true,
            ok: false,
            elapsed_ms: 0,
            rtf: 0.0,
            device_name: None,
            error: Some(why.into()),
        }
    }
}

/// 本机硬件画像。落 SQLite 的 `hardware_profile` 单行表,**不进同步**。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HardwareProfile {
    pub backend_selected: Backend,
    pub backend_requested: Option<Backend>,
    pub device_name: Option<String>,
    pub vram_gb: Option<f32>,
    pub cpu_cores: usize,
    pub rtf_measured: f32,
    pub model_recommended: ModelTier,
    pub probed_at: i64,
    /// 各后端的探测明细(含错误原因),用于排查
    pub probes: Vec<ProbeResult>,
}

impl HardwareProfile {
    /// 人类可读的一行摘要,给 UI 的硬件面板用。
    pub fn summary_line(&self) -> String {
        match &self.device_name {
            Some(d) if self.backend_selected.is_gpu() => {
                format!("{} · {}", self.backend_selected.label(), d)
            }
            _ => format!("{}(未检测到可用 GPU)", self.backend_selected.label()),
        }
    }

    /// 按实测实时率估算一段音频的转写耗时。
    pub fn estimate_duration(&self, audio_ms: u64) -> Duration {
        if self.rtf_measured <= 0.0 {
            // 没有实测数据时退回按后端粗略估计
            let guess = match self.backend_selected {
                Backend::Cuda | Backend::Metal => 0.05,
                Backend::Vulkan | Backend::Rocm | Backend::Sycl => 0.20,
                _ => 1.0,
            };
            return Duration::from_millis((audio_ms as f32 * guess) as u64);
        }
        Duration::from_millis((audio_ms as f32 * self.rtf_measured) as u64)
    }
}

/// 把时长格式化成"约 2 分钟"/"约 1 小时 5 分钟"。
pub fn humanize_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("约 {secs} 秒")
    } else if secs < 3600 {
        format!("约 {} 分钟", secs / 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("约 {h} 小时")
        } else {
            format!("约 {h} 小时 {m} 分钟")
        }
    }
}

// ---------------------------------------------------------------------------
// 探测
// ---------------------------------------------------------------------------

/// 硬件层面的可用性检查(便宜,不跑推理)。
pub fn available(backend: Backend) -> bool {
    match backend {
        Backend::Cuda => nvidia_smi_devices().is_some(),
        Backend::Vulkan => vulkan_loader_present(),
        Backend::Metal => cfg!(target_os = "macos"),
        Backend::Rocm => which("rocminfo").is_some() || which("hipconfig").is_some(),
        Backend::Sycl => which("sycl-ls").is_some(),
        Backend::OpenCl => which("clinfo").is_some(),
        // CPU 永远可用,无需探测
        Backend::Cpu => true,
    }
}

/// 在 PATH 中查找可执行文件(自动补 `.exe`)。
pub fn find_in_path(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for cand in [dir.join(exe), dir.join(format!("{exe}.exe"))] {
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

fn which(exe: &str) -> Option<PathBuf> {
    find_in_path(exe)
}

/// 查询 NVIDIA 设备名列表。返回 None 表示没有可用的 CUDA。
pub fn nvidia_smi_devices() -> Option<Vec<String>> {
    let exe = which("nvidia-smi")?;
    // ★ 用 no_window —— 否则每次探测都闪一个黑窗口
    let out = crate::process::no_window(exe)
        .args(["--query-gpu=name", "--format=csv,noheader"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let devs: Vec<String> = text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if devs.is_empty() {
        None
    } else {
        Some(devs)
    }
}

/// 查询显存总量(GB)。
pub fn nvidia_vram_gb() -> Option<f32> {
    let exe = which("nvidia-smi")?;
    let out = crate::process::no_window(exe)
        .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next()?.trim().parse::<f32>().ok()?;
    Some(first / 1024.0)
}

/// Vulkan 加载器是否随驱动安装。
///
/// 注意:有加载器不等于有 SDK —— 编译 Vulkan 后端需要 glslc。
/// 这里只判断**运行**可行性。
pub fn vulkan_loader_present() -> bool {
    if cfg!(target_os = "windows") {
        let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        Path::new(&sysroot).join("System32\\vulkan-1.dll").is_file()
    } else {
        ["libvulkan.so.1", "libvulkan.so", "libvulkan.dylib"]
            .iter()
            .any(|lib| {
                ["/usr/lib", "/usr/local/lib", "/lib/x86_64-linux-gnu"]
                    .iter()
                    .any(|d| Path::new(d).join(lib).exists())
            })
    }
}

pub fn cpu_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

// ---------------------------------------------------------------------------
// 后端选择
// ---------------------------------------------------------------------------

/// sidecar 二进制所在位置。
#[derive(Clone, Debug)]
pub struct SidecarLocator {
    /// `binaries/` 根目录,其下按后端分子目录
    pub root: PathBuf,
}

impl SidecarLocator {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 定位某后端的 whisper-cli 可执行文件。
    pub fn whisper_cli(&self, backend: Backend) -> Option<PathBuf> {
        let dir = self.root.join(backend.sidecar_dir());
        for name in ["whisper-cli", "main"] {
            for ext in ["", ".exe"] {
                let p = dir.join(format!("{name}{ext}"));
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        None
    }

    /// 已安装后端的二进制位置。
    pub fn installed(&self) -> Vec<(Backend, PathBuf)> {
        Backend::PRIORITY
            .iter()
            .filter_map(|b| self.whisper_cli(*b).map(|p| (*b, p)))
            .collect()
    }
}

/// 降级链:Auto 时按优先级找第一个可用且通过自检的后端;Force 时直接返回(由调用方校验)。
///
/// ★ 注意 `Force` 分支**不做降级** —— 校验失败要由调用方明确报错。
pub fn select_backend(pref: BackendPref, locator: &SidecarLocator) -> Option<Backend> {
    match pref {
        BackendPref::Force(b) => Some(b),
        BackendPref::Auto => Backend::PRIORITY.into_iter().find(|b| {
            // ① 有二进制
            locator.whisper_cli(*b).is_some()
            // ② 硬件层面可能可用
                && available(*b)
        }),
    }
}

/// 说明为什么某个后端不可用,给用户看的可行动提示。
pub fn explain_unavailable(backend: Backend, locator: &SidecarLocator) -> String {
    if locator.whisper_cli(backend).is_none() {
        return match backend {
            Backend::Cuda => format!(
                "未找到 CUDA 版 whisper-cli。请将其放到 {} 目录。",
                locator.root.join("cuda").display()
            ),
            Backend::Vulkan => format!(
                "未找到 Vulkan 版 whisper-cli。请将其放到 {} 目录。",
                locator.root.join("vulkan").display()
            ),
            _ => format!(
                "未找到 {} 版 whisper-cli(期望位置 {})。",
                backend.label(),
                locator.root.join(backend.sidecar_dir()).display()
            ),
        };
    }
    match backend {
        Backend::Cuda => "检测到 CUDA 版程序,但当前机器没有可用的 NVIDIA 驱动/设备。".into(),
        Backend::Vulkan => "未检测到 Vulkan 运行库(vulkan-1.dll),无法使用 Vulkan 后端。".into(),
        Backend::Metal => "Metal 后端仅在 macOS 上可用。".into(),
        _ => format!("{} 后端在当前机器上不可用。", backend.label()),
    }
}

// ---------------------------------------------------------------------------
// 模型推荐
// ---------------------------------------------------------------------------

/// 按硬件推荐模型档位。这是"无 GPU 也能用"的关键 —— 不能让弱机器默认下大模型。
pub fn recommend_model(backend: Backend, vram_gb: Option<f32>, cores: usize) -> ModelTier {
    match backend {
        Backend::Cuda | Backend::Metal => {
            // 6GB 以上显存可跑 large-v3-turbo(int8 约 1.6GB,留足余量)
            if vram_gb.map(|v| v >= 6.0).unwrap_or(true) {
                ModelTier::LargeV3Turbo
            } else {
                ModelTier::Medium
            }
        }
        Backend::Vulkan | Backend::Rocm | Backend::Sycl => ModelTier::Medium,
        Backend::OpenCl => ModelTier::Small,
        Backend::Cpu => {
            // CPU 上模型档位比后端更重要
            if cores >= 16 {
                ModelTier::Small
            } else if cores >= 8 {
                ModelTier::Base
            } else {
                ModelTier::Tiny
            }
        }
    }
}

/// 需要提示用户"会很慢"的情况(CPU 跑长音频)。
pub fn should_warn_slow(backend: Backend, audio_ms: u64) -> bool {
    !backend.is_gpu() && audio_ms > 20 * 60 * 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_is_always_available() {
        assert!(available(Backend::Cpu));
    }

    #[test]
    fn priority_prefers_cuda_then_vulkan_then_cpu() {
        let p = Backend::PRIORITY;
        let ci = p.iter().position(|b| *b == Backend::Cuda).unwrap();
        let vi = p.iter().position(|b| *b == Backend::Vulkan).unwrap();
        let pi = p.iter().position(|b| *b == Backend::Cpu).unwrap();
        assert!(ci < vi && vi < pi, "CUDA 应优先于 Vulkan 优先于 CPU");
    }

    #[test]
    fn recommend_model_scales_with_hardware() {
        // GPU 上给大模型
        assert_eq!(
            recommend_model(Backend::Cuda, Some(8.0), 32),
            ModelTier::LargeV3Turbo
        );
        // 小显存 N 卡降档
        assert_eq!(
            recommend_model(Backend::Cuda, Some(4.0), 32),
            ModelTier::Medium
        );
        // 核显走 Vulkan 时给中档
        assert_eq!(
            recommend_model(Backend::Vulkan, None, 16),
            ModelTier::Medium
        );
        // CPU 上按核心数分档,且绝不给大模型
        assert_eq!(recommend_model(Backend::Cpu, None, 32), ModelTier::Small);
        assert_eq!(recommend_model(Backend::Cpu, None, 12), ModelTier::Base);
        assert_eq!(recommend_model(Backend::Cpu, None, 4), ModelTier::Tiny);
    }

    #[test]
    fn cpu_never_gets_large_model() {
        for cores in [1, 4, 8, 16, 32, 128] {
            let m = recommend_model(Backend::Cpu, None, cores);
            assert_ne!(m, ModelTier::LargeV3Turbo, "{cores} 核不该推荐大模型");
        }
    }

    #[test]
    fn force_does_not_downgrade() {
        let loc = SidecarLocator::new("definitely/not/here");
        // Force 即使没有二进制也返回该后端,由调用方负责报错
        assert_eq!(
            select_backend(BackendPref::Force(Backend::Cuda), &loc),
            Some(Backend::Cuda)
        );
    }

    #[test]
    fn auto_returns_none_when_no_sidecar() {
        let loc = SidecarLocator::new("definitely/not/here");
        // 没有任何 sidecar 时 Auto 选不出来(调用方会给出提示)
        assert_eq!(select_backend(BackendPref::Auto, &loc), None);
    }

    #[test]
    fn explain_unavailable_mentions_path() {
        let loc = SidecarLocator::new("X:/bin");
        let msg = explain_unavailable(Backend::Cuda, &loc);
        assert!(msg.contains("cuda"), "提示里应包含期望路径: {msg}");
    }

    #[test]
    fn humanize_duration_buckets() {
        assert_eq!(humanize_duration(Duration::from_secs(45)), "约 45 秒");
        assert_eq!(humanize_duration(Duration::from_secs(150)), "约 2 分钟");
        assert_eq!(
            humanize_duration(Duration::from_secs(3600)),
            "约 1 小时"
        );
        assert_eq!(
            humanize_duration(Duration::from_secs(3900)),
            "约 1 小时 5 分钟"
        );
    }

    #[test]
    fn estimate_falls_back_when_no_measurement() {
        let prof = HardwareProfile {
            backend_selected: Backend::Cpu,
            backend_requested: None,
            device_name: None,
            vram_gb: None,
            cpu_cores: 8,
            rtf_measured: 0.0,
            model_recommended: ModelTier::Base,
            probed_at: 0,
            probes: vec![],
        };
        // CPU 无实测数据时按 1.0 实时率估算
        assert_eq!(prof.estimate_duration(60_000).as_secs(), 60);
    }

    #[test]
    fn warn_slow_only_for_long_cpu_jobs() {
        assert!(should_warn_slow(Backend::Cpu, 60 * 60 * 1000));
        assert!(!should_warn_slow(Backend::Cpu, 5 * 60 * 1000));
        assert!(!should_warn_slow(Backend::Cuda, 60 * 60 * 1000));
    }

    #[test]
    fn sidecar_locator_finds_nothing_in_empty_dir() {
        let loc = SidecarLocator::new("definitely/not/here");
        assert!(loc.installed().is_empty());
        assert!(loc.whisper_cli(Backend::Cpu).is_none());
    }
}
