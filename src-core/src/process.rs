//! 启动子进程的统一入口。
//!
//! # 为什么需要这个模块
//!
//! Windows 上,从一个 **GUI 程序**(PE Subsystem = 2)里启动一个
//! **控制台程序**(PE Subsystem = 3)时,系统会为子进程分配一个控制台 ——
//! 也就是**闪一个黑窗口**。
//!
//! 本程序会启动这些子进程:
//!
//! | 子进程 | 用途 |
//! |---|---|
//! | `nvidia-smi` | 探测 CUDA 设备名与显存 |
//! | `whisper-cli` | 真正的转写 |
//! | `ffmpeg` | 解码非 WAV 音频 |
//!
//! 每次启动都弹一次窗口,而且探测阶段会连跑几次 —— 用户看到的就是
//! "弹几个窗口然后迅速关闭"。
//!
//! # 解法
//!
//! 给 `Command` 加 `CREATE_NO_WINDOW`(0x08000000)。它告诉系统
//! "这个子进程不需要控制台",于是不分配、不显示。
//!
//! **只影响窗口是否显示,不影响子进程的 stdin/stdout/stderr 管道** ——
//! 我们照样能读到 `nvidia-smi` 的输出和 `whisper-cli` 的错误信息。
//!
//! # 别的平台
//!
//! 非 Windows 平台没有这个问题(没有"控制台子系统"的概念),
//! 这层包装是空操作。

use std::process::Command;

/// Windows: `CREATE_NO_WINDOW`。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 构造一个**不会弹窗口**的命令。
///
/// 所有启动子进程的地方都该用它,而不是直接 `Command::new`。
///
/// ```ignore
/// let out = no_window("nvidia-smi")
///     .args(["--query-gpu=name", "--format=csv,noheader"])
///     .output()?;
/// ```
pub fn no_window(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut c = Command::new(program);
    apply(&mut c);
    c
}

/// 给已有的 `Command` 加"不弹窗口"标志(就地修改)。
pub fn apply(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_window_builds_a_command() {
        let c = no_window("cmd");
        // 只验证能构造出来;真正的"不弹窗口"只能靠人工观察
        let _ = c;
    }

    #[test]
    fn apply_is_callable_on_existing_command() {
        let mut c = Command::new("cmd");
        apply(&mut c);
        let _ = c;
    }

    #[test]
    fn no_window_still_captures_output() {
        // ★ 关键回归:加了 CREATE_NO_WINDOW 之后,stdout 管道必须仍然可用。
        //   如果这一步坏了,探测和转写都会拿不到输出。
        #[cfg(windows)]
        let out = no_window("cmd")
            .args(["/C", "echo hello"])
            .output()
            .expect("应能启动 cmd");
        #[cfg(not(windows))]
        let out = no_window("sh")
            .args(["-c", "echo hello"])
            .output()
            .expect("应能启动 sh");

        assert!(out.status.success(), "退出状态应成功");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("hello"), "应能读到 stdout:{text:?}");
    }
}
