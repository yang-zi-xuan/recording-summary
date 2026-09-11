//! CLI 进度显示。
//!
//! 见技术方案 §3.4 —— **CPU 模式下转写可能跑一小时**,
//! 所以进度必须让用户能估算"还要多久",而不是只有一个转圈。
//! 这里用"已处理音频时长"而不是百分比,因为那是唯一能准确估算进度的量。

use rs_core::hardware::humanize_duration;
use rs_core::pipeline::{Progress, ProgressSink, Stage};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 构造一个往 stderr 打印进度的 sink。
pub fn cli_sink() -> ProgressSink {
    // 只在是终端时才输出动态进度,重定向到文件时保持干净
    let tty = is_stderr_tty();
    let last_stage = Arc::new(std::sync::Mutex::new(None::<Stage>));
    let printed_note = Arc::new(AtomicBool::new(false));

    ProgressSink::new(move |p| {
        match p {
            Progress::StageStart(stage, _) => {
                *last_stage.lock().unwrap() = Some(stage);
                if tty {
                    eprint!("\r\x1b[K▶ {} ...", stage.label());
                    let _ = std::io::stderr().flush();
                } else {
                    eprintln!("▶ {}", stage.label());
                }
            }
            Progress::StagePct(stage, pct) => {
                if !tty {
                    if pct >= 1.0 {
                        eprintln!("  ✓ {}", stage.label());
                    }
                    return;
                }
                if pct >= 1.0 {
                    eprint!("\r\x1b[K✓ {}\n", stage.label());
                } else {
                    eprint!("\r\x1b[K▶ {} ... {:.0}%", stage.label(), pct * 100.0);
                }
                let _ = std::io::stderr().flush();
            }
            Progress::Transcribe {
                audio_ms_done,
                audio_ms_total,
            } => {
                if !tty {
                    return;
                }
                let pct = if audio_ms_total > 0 {
                    audio_ms_done as f64 / audio_ms_total as f64
                } else {
                    0.0
                };
                // ★ 同时给出"已处理时长/总时长",用户据此估算剩余时间
                eprint!(
                    "\r\x1b[K▶ 转写 ... {:.0}%  ({} / {})",
                    pct * 100.0,
                    humanize_duration(Duration::from_millis(audio_ms_done)),
                    humanize_duration(Duration::from_millis(audio_ms_total)),
                );
                let _ = std::io::stderr().flush();
            }
            Progress::CacheHit(stage) => {
                if tty {
                    eprint!("\r\x1b[K⚡ {} 命中缓存\n", stage.label());
                } else {
                    eprintln!("⚡ {} 命中缓存", stage.label());
                }
                let _ = std::io::stderr().flush();
            }
            Progress::Note(msg) => {
                if tty {
                    eprint!("\r\x1b[K");
                }
                eprintln!("{msg}");
                printed_note.store(true, Ordering::Relaxed);
            }
        }
    })
}

fn is_stderr_tty() -> bool {
    // 不引入额外依赖:用环境变量粗判
    std::env::var_os("TERM").is_some() || atty_fallback()
}

fn atty_fallback() -> bool {
    // Windows 终端下 CONOUT$ 可打开
    #[cfg(windows)]
    {
        std::fs::OpenOptions::new()
            .write(true)
            .open("CONOUT$")
            .is_ok()
    }
    #[cfg(not(windows))]
    {
        true
    }
}
