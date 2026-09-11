//! 端到端集成测试:用假 LLM 跑通"文件 → 转写 → 场景 → 纪要"全链路。
//!
//! 这些测试**不依赖**:
//! - GPU / CUDA(用 MockTranscriber)
//! - 网络与 API Key(用 MockSummarizer)
//! - 几 GB 的模型文件
//!
//! 目的是证明**管线本身是通的**,而真实引擎的验证由 CLI 侧完成
//! (见 README 的"还需要你做的事")。

use rs_core::asr::MockTranscriber;
use rs_core::audio::{encode_wav_16, PcmAudio};
use rs_core::llm::{MockSummarizer, Summarizer};
use rs_core::pipeline::view::{self, ViewKind};
use rs_core::pipeline::{Pipeline, PipelineConfig, Progress, ProgressSink, Stage};
use rs_core::types::{
    AudioRef, AudioSourceKind, BackendPref, DiarizeMode, Scene, SpeakerLabels,
};
use std::sync::{Arc, Mutex};

/// 合成一段"有内容的"音频(不依赖 TTS 或外部素材)。
fn synth(ms: u64) -> PcmAudio {
    PcmAudio {
        samples: vec![0.05; (16_000u64 * ms / 1000) as usize],
        sample_rate: 16_000,
        channels: 1,
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    audio_path: std::path::PathBuf,
    cfg: PipelineConfig,
}

fn fixture(ms: u64) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let audio_path = dir.path().join("lecture.wav");
    encode_wav_16(&audio_path, &synth(ms)).unwrap();
    let cfg = PipelineConfig::new(dir.path().join("data"));
    Fixture {
        _dir: dir,
        audio_path,
        cfg,
    }
}

fn input(path: &std::path::Path) -> AudioRef {
    AudioRef {
        path: path.to_path_buf(),
        duration_ms: None,
        content_hash: None,
        source: AudioSourceKind::File,
    }
}

#[test]
fn full_pipeline_end_to_end() {
    let f = fixture(120_000); // 2 分钟
    let tc = MockTranscriber::new();
    let sum = MockSummarizer::new();
    let pipe = Pipeline::new(f.cfg.clone(), &tc, Some(&sum)).unwrap();

    let out = pipe.run(&input(&f.audio_path), &ProgressSink::silent()).unwrap();

    // 转写
    assert!(!out.transcript.segments.is_empty());
    assert_eq!(out.transcript.duration_ms, 120_000);
    assert_eq!(out.transcript.backend, rs_core::types::Backend::Cpu);

    // 场景
    let scene = out.scene.as_ref().expect("应有场景判断结果");
    assert!(scene.confidence > 0.0);

    // 纪要
    let s = out.summary.as_ref().expect("应有纪要");
    assert!(!s.content_md.is_empty());
    assert_eq!(s.scene, scene.scene);

    // 落盘
    assert!(pipe.files().has_transcript(&out.session_id));
    assert!(pipe.files().read_labels(&out.session_id).unwrap().is_some());
    assert!(pipe
        .files()
        .read_summary_markdown(&out.session_id)
        .unwrap()
        .is_some());
}

#[test]
fn all_three_views_render_from_same_data() {
    let f = fixture(30_000);
    let tc = MockTranscriber::new();
    let pipe = Pipeline::new(f.cfg.clone(), &tc, None).unwrap();
    let out = pipe.run(&input(&f.audio_path), &ProgressSink::silent()).unwrap();

    // Mock 转写不含说话人信息(真实路径下由 diarization 填入),
    // 这里手工标注,以便验证"同一份数据派生三种视图"。
    let mut segments = out.transcript.segments.clone();
    assert!(segments.len() >= 2);
    let mid = segments.len() / 2;
    for (i, s) in segments.iter_mut().enumerate() {
        s.speaker_id = Some(if i < mid { 0 } else { 1 });
    }

    let mut labels = SpeakerLabels::new(&out.session_id);
    labels.ensure([0, 1]);
    assert!(labels.rename(0, "张老师"), "标签应已存在并可改名");
    assert!(labels.rename(1, "李同学"));

    let dialogue = view::render_dialogue(&segments, &labels);
    let timeline = view::render_timeline(&segments, &labels);
    let plain = view::render_plain(&segments, &labels);
    let srt = view::render_srt(&segments, &labels);

    assert!(dialogue.contains("张老师"), "{dialogue}");
    assert!(timeline.contains("张老师"), "{timeline}");
    assert!(plain.contains("张老师:"), "{plain}");
    assert!(srt.contains("-->"), "{srt}");

    // ★ 三种视图必须来自同一份数据
    let merged = view::merge_for_dialogue_default(&segments);
    assert!(!merged.is_empty());
    // 同一说话人的相邻段落应被合并,因此条数少于原始段落数
    assert!(
        merged.len() < segments.len(),
        "对话体应合并同说话人的相邻段落:{} vs {}",
        merged.len(),
        segments.len()
    );
    // 两个说话人都必须出现在结果里
    assert!(merged.iter().any(|u| u.speaker_id == Some(0)));
    assert!(merged.iter().any(|u| u.speaker_id == Some(1)));
}

#[test]
fn rename_propagates_and_marks_summary_stale() {
    let f = fixture(60_000);
    let tc = MockTranscriber::new();
    let sum = MockSummarizer::new();
    let pipe = Pipeline::new(f.cfg.clone(), &tc, Some(&sum)).unwrap();
    let out = pipe.run(&input(&f.audio_path), &ProgressSink::silent()).unwrap();

    // 造一个带说话人的标签集合并落盘
    let mut labels = out.labels.clone();
    labels.ensure([0]);
    labels.rename(0, "张老师");
    pipe.files().write_labels(&labels).unwrap();

    // 重新生成纪要 —— 应引用新名字
    let scene = out.scene.as_ref().map(|v| v.scene).unwrap_or(Scene::Lecture);
    let s2 = sum
        .summarize(&out.transcript, scene, &labels, labels.labels_version)
        .unwrap();
    assert!(s2.content_md.contains("张老师"), "{}", s2.content_md);

    // 再改名 → 纪要应被判为过时
    labels.rename(0, "李老师");
    assert!(s2.is_stale(&labels), "★ 改名后总结必须被识别为过时");

    // 旧纪要文件仍在,但 meta 里的版本号落后
    let meta = pipe.files().read_summary_meta(&out.session_id).unwrap();
    if let Some(m) = meta {
        // 首次生成的纪要用的是版本 0
        assert!(m.labels_version < labels.labels_version);
    }
}

#[test]
fn cache_makes_second_run_fast() {
    let f = fixture(90_000);
    let tc = MockTranscriber::new();
    let sum = MockSummarizer::new();
    let pipe = Pipeline::new(f.cfg.clone(), &tc, Some(&sum)).unwrap();

    let first = pipe.run(&input(&f.audio_path), &ProgressSink::silent()).unwrap();
    assert!(!first.transcript_from_cache);

    let second = pipe.run(&input(&f.audio_path), &ProgressSink::silent()).unwrap();
    assert!(second.transcript_from_cache, "同一音频第二次必须命中缓存");
    assert_eq!(
        first.transcript.raw_text, second.transcript.raw_text,
        "缓存结果必须与首次一致"
    );
}

#[test]
fn changing_diarize_count_invalidates_cache() {
    let f = fixture(30_000);
    let tc = MockTranscriber::new();

    let mut c1 = f.cfg.clone();
    c1.diarize = DiarizeMode::Fixed(3);
    let p1 = Pipeline::new(c1, &tc, None).unwrap();
    assert!(!p1
        .run(&input(&f.audio_path), &ProgressSink::silent())
        .unwrap()
        .transcript_from_cache);

    let mut c2 = f.cfg.clone();
    c2.diarize = DiarizeMode::Fixed(5);
    let p2 = Pipeline::new(c2, &tc, None).unwrap();
    assert!(
        !p2.run(&input(&f.audio_path), &ProgressSink::silent())
            .unwrap()
            .transcript_from_cache,
        "★ 改发言人数必须重新转写(缓存键含 num_speakers)"
    );
}

#[test]
fn progress_reports_every_stage_in_order() {
    let f = fixture(30_000);
    let tc = MockTranscriber::new();
    let sum = MockSummarizer::new();
    let pipe = Pipeline::new(f.cfg.clone(), &tc, Some(&sum)).unwrap();

    let events: Arc<Mutex<Vec<Stage>>> = Arc::new(Mutex::new(vec![]));
    let notes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let e2 = Arc::clone(&events);
    let n2 = Arc::clone(&notes);

    let sink = ProgressSink::new(move |p| match p {
        Progress::StageStart(s, _) => e2.lock().unwrap().push(s),
        Progress::Note(m) => n2.lock().unwrap().push(m),
        _ => {}
    });

    pipe.run(&input(&f.audio_path), &sink).unwrap();

    let stages = events.lock().unwrap().clone();
    let pos = |s: Stage| stages.iter().position(|x| *x == s);
    assert!(pos(Stage::Probe) < pos(Stage::Decode));
    assert!(pos(Stage::Decode) < pos(Stage::Transcribe));
    assert!(pos(Stage::Transcribe) < pos(Stage::Persist));

    // 必须有给用户看的提示(硬件、时长、段落数等)
    let ns = notes.lock().unwrap();
    assert!(ns.iter().any(|m| m.contains("硬件")), "{ns:?}");
    assert!(ns.iter().any(|m| m.contains("音频时长")), "{ns:?}");
}

#[test]
fn cancellation_leaves_partial_state_recoverable() {
    let f = fixture(60_000);
    let tc = MockTranscriber::new();
    let pipe = Pipeline::new(f.cfg.clone(), &tc, None).unwrap();

    let sink = ProgressSink::new(|_| {});
    sink.canceller().cancel();

    let err = pipe.run(&input(&f.audio_path), &sink);
    assert!(err.is_err());

    // 取消后数据目录应当仍然是可用状态(能再开一次)
    let pipe2 = Pipeline::new(f.cfg.clone(), &tc, None).unwrap();
    let ok = pipe2.run(&input(&f.audio_path), &ProgressSink::silent());
    assert!(ok.is_ok(), "★ 取消后必须能重新运行,不能留下坏状态");
}

#[test]
fn long_audio_triggers_map_reduce_path() {
    // MockSummarizer 不走 map-reduce,但我们可以直接验证 prompt 侧的切分
    let text: String = (0..3000)
        .map(|i| format!("这是第{i}句话,用来填充长度。"))
        .collect::<Vec<_>>()
        .join("\n");
    let chunks = rs_core::llm::prompt::split_for_map_reduce(&text);
    assert!(chunks.len() > 1, "长文本应触发 map-reduce 分段");
    // 每段都不应超过单次请求的合理规模
    for c in &chunks {
        assert!(
            rs_core::llm::prompt::estimate_tokens(c) < 12_000,
            "单段过大,会导致请求超限"
        );
    }
}

#[test]
fn view_kind_parse_covers_all_variants() {
    for (s, k) in [
        ("dialogue", ViewKind::Dialogue),
        ("timeline", ViewKind::Timeline),
        ("plain", ViewKind::Plain),
        ("srt", ViewKind::Srt),
    ] {
        assert_eq!(ViewKind::parse(s), Some(k));
    }
}

#[test]
fn explicit_backend_survives_pipeline() {
    let f = fixture(10_000);
    let mut cfg = f.cfg.clone();
    cfg.backend_pref = BackendPref::Force(rs_core::types::Backend::Cuda);
    let tc = MockTranscriber::new();
    let pipe = Pipeline::new(cfg, &tc, None).unwrap();
    let out = pipe.run(&input(&f.audio_path), &ProgressSink::silent()).unwrap();
    assert_eq!(
        out.hardware.unwrap().backend_selected,
        rs_core::types::Backend::Cuda,
        "★ 手动指定后端不能被静默改写"
    );
    assert_eq!(out.transcript.backend, rs_core::types::Backend::Cuda);
}

#[test]
fn missing_models_do_not_break_the_run() {
    // 这正是真实场景:声纹模型没下载时,转写和纪要必须照常产出
    let f = fixture(20_000);
    let tc = MockTranscriber::new();
    let sum = MockSummarizer::new();
    let pipe = Pipeline::new(f.cfg.clone(), &tc, Some(&sum)).unwrap();
    let out = pipe.run(&input(&f.audio_path), &ProgressSink::silent()).unwrap();

    assert!(out.transcript.diarize.is_none());
    assert!(!out.transcript.segments.is_empty());
    assert!(out.summary.is_some());
}

#[test]
fn summarizer_trait_is_usable_via_dyn() {
    // 管线内部用 &dyn Summarizer,这条验证 trait 对象确实可构造
    let sum = MockSummarizer::new();
    let dyn_sum: &dyn Summarizer = &sum;
    let f = fixture(5_000);
    let tc = MockTranscriber::new();
    let pipe = Pipeline::new(f.cfg.clone(), &tc, Some(dyn_sum)).unwrap();
    let out = pipe.run(&input(&f.audio_path), &ProgressSink::silent()).unwrap();
    assert!(out.summary.is_some());
}
