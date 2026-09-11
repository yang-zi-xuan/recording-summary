/* 录音转总结 —— 前端逻辑
 *
 * 与 Rust 侧的契约:
 *   命令:invoke("<name>", { args…})
 *   事件:pipeline://progress · pipeline://done · pipeline://error · sync://progress
 *
 * 全部业务逻辑在 rs-core,Rust 侧只做分发。所以这里尽量薄。
 */

// ---------------------------------------------------------------------------
// Tauri API 接入
//
// ⚠️ 这里刻意用"延迟解析"而不是顶层的 `const { invoke } = …`:
//    后者在 __TAURI__ 未注入时会在**脚本加载阶段**就抛异常,
//    导致整个 app.js 失效 —— 而界面上只会看到"数据一直加载不出来"。
//    延迟到调用时解析,才能把错误显示给用户。
// ---------------------------------------------------------------------------

function tauriCore() {
  const t = window.__TAURI__;
  if (!t) throw new Error('未检测到 window.__TAURI__(Tauri 全局 API 未注入)');
  if (!t.core || typeof t.core.invoke !== 'function') {
    throw new Error('window.__TAURI__.core.invoke 不存在(API 版本不匹配)');
  }
  return t.core;
}

function tauriEvent() {
  const t = window.__TAURI__;
  if (!t || !t.event || typeof t.event.listen !== 'function') {
    throw new Error('window.__TAURI__.event.listen 不存在');
  }
  return t.event;
}

function tauriDialog() {
  return window.__TAURI__ ? window.__TAURI__.dialog : undefined;
}

function tauriWebview() {
  const t = window.__TAURI__;
  return t && t.webview ? t.webview : undefined;
}

async function invoke(cmd, args) {
  return tauriCore().invoke(cmd, args);
}

async function listen(evt, cb) {
  return tauriEvent().listen(evt, cb);
}

async function openDialog(opts) {
  const d = tauriDialog();
  if (!d) throw new Error('文件对话框插件未就绪');
  return d.open(opts);
}

async function saveDialog(opts) {
  const d = tauriDialog();
  if (!d) throw new Error('文件对话框插件未就绪');
  return d.save(opts);
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

const $ = (id) => document.getElementById(id);
const show = (el) => el && el.classList.remove('hidden');
const hide = (el) => el && el.classList.add('hidden');

function fmtBytes(n) {
  if (n < 1024) return n + ' B';
  if (n < 1048576) return (n / 1024).toFixed(1) + ' KB';
  return (n / 1048576).toFixed(1) + ' MB';
}

function fmtClock(ms) {
  const t = Math.floor(ms / 1000);
  const h = Math.floor(t / 3600);
  const m = Math.floor((t % 3600) / 60);
  const s = t % 60;
  const p = (x) => String(x).padStart(2, '0');
  return h > 0 ? `${h}:${p(m)}:${p(s)}` : `${m}:${p(s)}`;
}

function fmtTime(ts) {
  if (!ts) return '';
  const d = new Date(ts);
  const p = (x) => String(x).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** 极简 Markdown 渲染。不引第三方库,只覆盖后端实际会产出的结构。 */
function renderMd(md) {  if (!md) return '<p class="muted">(没有纪要)</p>';
  const esc = (s) =>
    s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

  const lines = md.split('\n');
  let out = [];
  let inUl = false;
  let inTable = false;

  const closeList = () => { if (inUl) { out.push('</ul>'); inUl = false; } };
  const closeTable = () => { if (inTable) { out.push('</tbody></table>'); inTable = false; } };

  const inline = (s) =>
    esc(s)
      .replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>')
      .replace(/`(.+?)`/g, '<code>$1</code>');

  for (let raw of lines) {
    const line = raw.replace(/\s+$/, '');

    // 表格
    if (line.startsWith('|') && line.endsWith('|')) {
      const cells = line.slice(1, -1).split('|').map((c) => c.trim());
      if (/^[-:\s|]+$/.test(line)) continue; // 分隔行
      if (!inTable) {
        closeList();
        out.push('<table><thead><tr>' + cells.map((c) => `<th>${inline(c)}</th>`).join('') + '</tr></thead><tbody>');
        inTable = true;
      } else {
        out.push('<tr>' + cells.map((c) => `<td>${inline(c)}</td>`).join('') + '</tr>');
      }
      continue;
    } else {
      closeTable();
    }

    // 标题
    const h = line.match(/^(#{1,6})\s+(.*)$/);
    if (h) {
      closeList();
      const lvl = Math.min(h[1].length + 1, 4);
      out.push(`<h${lvl}>${inline(h[2])}</h${lvl}>`);
      continue;
    }

    // 列表
    const li = line.match(/^\s*[-*+]\s+(.*)$/);
    if (li) {
      if (!inUl) { out.push('<ul>'); inUl = true; }
      out.push(`<li>${inline(li[1])}</li>`);
      continue;
    }
    closeList();

    if (line.trim() === '') continue;
    out.push(`<p>${inline(line)}</p>`);
  }
  closeList();
  closeTable();
  return out.join('\n');
}

function logTo(el, msg, cls) {
  if (!el) return;
  const span = document.createElement('div');
  if (cls) span.className = cls;
  span.textContent = msg;
  el.appendChild(span);
  el.scrollTop = el.scrollHeight;
}

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

const S = {
  pickedPath: null,
  hardware: null,
  currentSession: null,
  currentDetail: null,
  running: false,
};

// ---------------------------------------------------------------------------
// 视图切换
// ---------------------------------------------------------------------------

document.querySelectorAll('.nav-item').forEach((b) => {
  b.addEventListener('click', () => {
    const v = b.dataset.view;
    document.querySelectorAll('.nav-item').forEach((x) => x.classList.remove('active'));
    b.classList.add('active');
    document.querySelectorAll('.view').forEach((x) => x.classList.remove('active'));
    $('view-' + v).classList.add('active');

    if (v === 'history') loadHistory();
    if (v === 'projects') loadProjects();
    if (v === 'profiles') loadProfiles();
    if (v === 'sync') loadSync();
    if (v === 'settings') loadSettings();
  });
});

function gotoView(name) {
  const btn = document.querySelector(`.nav-item[data-view="${name}"]`);
  if (btn) btn.click();
}

// ---------------------------------------------------------------------------
// 硬件面板
// ---------------------------------------------------------------------------

async function loadHardware() {
  try {
    const hw = await invoke('probe_hardware');
    S.hardware = hw;

    const w = $('hwCard');
    const dev = hw.device_name ? ` · ${hw.device_name}` : '';
    w.innerHTML = `
      <div class="hw-row"><span>后端</span><b>${hw.backend_label}${dev}</b></div>
      <div class="hw-row"><span>模型</span><b>${hw.model_recommended}</b></div>
      <div class="hw-row"><span>CPU</span><b>${hw.cpu_cores} 核</b></div>
      ${hw.vram_gb ? `<div class="hw-row"><span>显存</span><b>${hw.vram_gb.toFixed(1)} GB</b></div>` : ''}
      ${!hw.model_ready ? `<div class="hw-warn">⚠ 模型 ${hw.model_file} 未下载</div>` : ''}
      ${!hw.ffmpeg ? `<div class="hw-warn">⚠ 未找到 ffmpeg,只能处理 wav</div>` : ''}
    `;

    // 设置页的详细面板
    const detail = $('hwDetail');
    if (detail) {
      const rows = hw.backends
        .map((b) => {
          const mark = b.usable ? '✅' : b.hardware_available ? '⚠' : '—';
          const why = b.usable
            ? '可用'
            : b.hardware_available
            ? '硬件支持但缺少程序'
            : '不支持';
          return `<div class="hw-row"><span>${mark} ${b.label}</span><b>${why}</b></div>`;
        })
        .join('');
      detail.innerHTML = `
        <div class="hw-row"><span>数据目录</span><b style="font-size:11px">${hw.data_dir}</b></div>
        <div class="hw-row"><span>二进制目录</span><b style="font-size:11px">${hw.binaries_dir}</b></div>
        <div class="hw-row"><span>模型目录</span><b style="font-size:11px">${hw.models_dir}</b></div>
        <div class="hw-row"><span>ffmpeg</span><b style="font-size:11px">${hw.ffmpeg || '未找到'}</b></div>
        <hr style="border:0;border-top:1px solid var(--line);margin:10px 0">
        ${rows}
      `;
    }

    // 模型下拉
    const models = await invoke('list_models');
    const sel = $('optModel');
    sel.innerHTML = '<option value="">按硬件自动</option>';
    models.forEach((m) => {
      const label = m.present
        ? `${m.tier} (${m.size_mb} MB)`
        : `${m.tier} — 未下载 (约 ${m.approx_mb} MB)`;
      const o = document.createElement('option');
      o.value = m.tier;
      o.textContent = label;
      o.disabled = !m.present;
      // 推荐档位默认选中
      if (m.tier === hw.model_recommended && m.present) o.selected = true;
      sel.appendChild(o);
    });
  } catch (e) {
    $('hwCard').innerHTML = `<div class="hw-warn">探测失败:${e}</div>`;
  }
}

// ---------------------------------------------------------------------------
// 处理
// ---------------------------------------------------------------------------

const dz = $('dropzone');

/** 拖放接入。
 *
 *  ⚠️ Tauri 2 的窗口默认 `dragDropEnabled: true`,它在**窗口层拦截**原生
 *  拖放,所以 HTML5 的 dragenter/dragover/drop **根本不会触发** ——
 *  之前的实现挂在这些事件上,等于从来没生效过。
 *
 *  正确的做法是监听 Tauri 的原生拖放事件
 *  (`getCurrentWebview().onDragDropEvent()`),它直接给出**文件绝对路径**。
 *
 *  顺带修掉另一个错:`dataTransfer.files[0].path` 是 Tauri 1 / Electron 的
 *  写法,Tauri 2 里不存在(标准 File 对象只有 name,没有 path)。
 */
async function setupDragDrop() {
  const wv = tauriWebview();
  DRAG_DROP.detail = wv
    ? typeof wv.getCurrentWebview === 'function'
      ? '有 webview.getCurrentWebview'
      : 'webview 存在但没有 getCurrentWebview'
    : 'window.__TAURI__.webview 不存在';

  if (wv && typeof wv.getCurrentWebview === 'function') {
    try {
      await wv.getCurrentWebview().onDragDropEvent((ev) => {
        const p = ev.payload;
        DRAG_DROP.events++;
        DRAG_DROP.lastType = p.type;
        if (p.type === 'over') {
          dz.classList.add('over');
        } else if (p.type === 'drop') {
          dz.classList.remove('over');
          const paths = p.paths || [];
          DRAG_DROP.lastPaths = paths;
          if (paths.length) setPicked(paths[0]);
        } else {
          // leave
          dz.classList.remove('over');
        }
      });
      DRAG_DROP.mode = 'native';
      DRAG_DROP.ready = true;
      return; // 原生事件可用,不必再挂 HTML5 兜底
    } catch (e) {
      DRAG_DROP.detail = '注册失败: ' + e;
      console.warn('[录音转总结] 原生拖放注册失败,退回 HTML5 拖放:', e);
    }
  }
  setupHtml5Drop();
  DRAG_DROP.mode = 'html5';
  DRAG_DROP.ready = true;
}

/** 拖放状态。既用于排查,也通过 window.__DRAG_DROP__ 暴露给 --diag 浮层。 */
const DRAG_DROP = {
  mode: '未初始化',
  ready: false,
  events: 0,
  lastType: '',
  lastPaths: [],
  detail: '',
};
window.__DRAG_DROP__ = DRAG_DROP;

/** HTML5 拖放兜底。
 *
 *  只在原生拖放不可用时走到这里。注意此时**拿不到绝对路径**
 *  (浏览器的 File 对象没有 path),只能提示用户改用「选择文件」。
 */
function setupHtml5Drop() {
  console.warn('[录音转总结] 使用 HTML5 拖放兜底 —— 可能拿不到完整路径');
  ['dragenter', 'dragover'].forEach((ev) =>
    dz.addEventListener(ev, (e) => {
      e.preventDefault();
      dz.classList.add('over');
    })
  );
  ['dragleave', 'drop'].forEach((ev) =>
    dz.addEventListener(ev, (e) => {
      e.preventDefault();
      dz.classList.remove('over');
    })
  );
  dz.addEventListener('drop', (e) => {
    const f = e.dataTransfer.files[0];
    if (!f) return;
    // Tauri 2 的 File 对象没有 path;能拿到就用,拿不到就让用户走文件对话框
    if (f.path) {
      setPicked(f.path);
    } else {
      alert(
        '拖放没能拿到文件的完整路径。\n\n' +
          '请点「选择文件」按钮挑选音频 —— 这个方式一定能拿到路径。'
      );
    }
  });
}

setupDragDrop();

$('btnPick').addEventListener('click', async () => {
  const p = await openDialog({
    multiple: false,
    filters: [
      { name: '音频/视频', extensions: ['wav', 'mp3', 'm4a', 'aac', 'mp4', 'flac', 'ogg', 'wma', 'opus'] },
      { name: '全部', extensions: ['*'] },
    ],
  });
  if (p) setPicked(typeof p === 'string' ? p : p.path);
});

function setPicked(p) {
  S.pickedPath = p;
  $('pickedFile').textContent = p ? '已选择:' + p : '';
  $('btnStart').disabled = !p;
}

$('btnStart').addEventListener('click', async () => {
  if (!S.pickedPath) return;
  const terms = $('optTerms').value
    .split(/[,,\s]+/)
    .map((s) => s.trim())
    .filter(Boolean);

  hide($('resultCard'));
  show($('progressCard'));
  $('progLog').innerHTML = '';
  $('progBar').style.width = '0%';
  $('progStage').textContent = '准备中…';
  $('progPct').textContent = '';
  $('progDetail').textContent = '';

  S.running = true;
  $('btnStart').disabled = true;
  show($('btnCancel'));

  try {
    await invoke('start_run', {
      req: {
        input: S.pickedPath,
        language: $('optLanguage').value || null,
        model: $('optModel').value || null,
        backend: null,
        speakers: $('optSpeakers').value ? parseInt($('optSpeakers').value) : null,
        no_diarize: $('optNoDiarize').checked,
        no_summary: $('optNoSummary').checked,
        terms,
      },
    });
  } catch (e) {
    logTo($('progLog'), '✗ ' + e, 'ln-warn');
    endRun();
  }
});

$('btnCancel').addEventListener('click', async () => {
  try {
    const ok = await invoke('cancel_run');
    logTo($('progLog'), ok ? '已请求取消,正在收尾…' : '当前没有任务', 'ln-warn');
  } catch (e) {
    logTo($('progLog'), '取消失败:' + e, 'ln-warn');
  }
});

async function endRun() {
  S.running = false;
  $('btnStart').disabled = !S.pickedPath;
  hide($('btnCancel'));
}

// ---- 事件订阅 ----

// 每个阶段的开始时刻,用于显示"已用多久"。
// 说话人区分这类阶段可能跑几分钟,只给一个进度条用户还是会怀疑卡死。
let stageStartedAt = 0;

listen('pipeline://progress', (ev) => {
  const p = ev.payload;
  if (p.kind === 'stage_start') {
    $('progStage').textContent = p.stage + '…';
    // ★ 进度条与明细必须归零。
    //
    //   之前的实现只换了阶段文字,进度条还停在上一阶段的 100% ——
    //   于是新阶段一开始界面就显示"100% 却不停",看起来像卡死。
    //   (实测:转写结束后进入说话人区分,用户以为程序挂了。)
    $('progBar').style.width = '0%';
    $('progPct').textContent = '';
    $('progDetail').textContent = '';
    stageStartedAt = Date.now();
  } else if (p.kind === 'stage_pct') {
    if (p.pct >= 1) {
      $('progPct').textContent = '';
      $('progDetail').textContent = '';
      logTo($('progLog'), '✓ ' + p.stage, 'ln-ok');
    } else {
      const pct = Math.max(0, Math.min(1, p.pct));
      $('progBar').style.width = Math.round(pct * 100) + '%';
      $('progPct').textContent = Math.round(pct * 100) + '%';
      // 同时给出已用时间 —— 进度不动时用户至少知道程序还活着
      const elapsed = Math.round((Date.now() - stageStartedAt) / 1000);
      $('progDetail').textContent = `${p.stage} 已用 ${fmtClock(elapsed * 1000)}`;
    }
  } else if (p.kind === 'transcribe') {
    const pct = p.total_ms ? p.done_ms / p.total_ms : 0;
    $('progBar').style.width = Math.round(pct * 100) + '%';
    $('progPct').textContent = Math.round(pct * 100) + '%';
    // 明细里同时给"音频进度"和"已用时间",后者对估算剩余时间更有用
    const elapsed = Math.round((Date.now() - stageStartedAt) / 1000);
    $('progDetail').textContent =
      '已处理 ' +
      fmtClock(p.done_ms) +
      ' / ' +
      fmtClock(p.total_ms) +
      `   已用 ${fmtClock(elapsed * 1000)}`;
  } else if (p.kind === 'cache_hit') {
    logTo($('progLog'), '⚡ ' + p.stage + ' 命中缓存', 'ln-ok');
  } else if (p.kind === 'note') {
    const cls = p.message.includes('⚠') ? 'ln-warn' : '';
    logTo($('progLog'), p.message, cls);
  }
});

listen('pipeline://done', (ev) => {
  const o = ev.payload;
  endRun();
  $('progBar').style.width = '100%';
  $('progStage').textContent = '完成';
  $('progPct').textContent = '';

  const chips = [
    `<span class="chip">${o.duration_text}</span>`,
    `<span class="chip">${o.segment_count} 段</span>`,
    `<span class="chip">${o.model}</span>`,
    `<span class="chip">${o.backend_label}</span>`,
  ];
  if (o.from_cache) chips.push('<span class="chip ok">缓存命中</span>');
  if (o.speaker_count) chips.push(`<span class="chip">${o.speaker_count} 位发言人</span>`);
  if (o.scene_label) {
    const cls = o.scene_low_confidence ? 'chip warn' : 'chip ok';
    const pct = o.scene_confidence ? Math.round(o.scene_confidence * 100) + '%' : '';
    chips.push(`<span class="${cls}">场景:${o.scene_label} ${pct}</span>`);
  }

  const card = $('resultCard');
  card.innerHTML = `
    <div class="result-head">
      <h3>处理完成</h3>
      <button class="btn ghost sm" id="btnOpenResult">查看详情</button>
    </div>
    <div class="chips">${chips.join('')}</div>
    <div class="md">${renderMd(o.summary || '(未生成纪要)')}</div>
  `;
  show(card);
  $('btnOpenResult').addEventListener('click', () => openSession(o.session_id));
});

listen('pipeline://error', (ev) => {
  endRun();
  logTo($('progLog'), '✗ ' + ev.payload, 'ln-warn');
  $('progStage').textContent = '失败';
});

// ---------------------------------------------------------------------------
// 我的工程
// ---------------------------------------------------------------------------

const MINDMAP = { zoom: 1, source: '', outline: '' };

/** Mermaid 初始化,只做一次。
 *
 *  为什么把 5.4MB 的 mermaid 内嵌进 ui/vendor 而不是用 CDN:
 *  这是桌面应用,必须离线可用。
 */
let mermaidReady = false;
function initMermaid() {
  if (mermaidReady) return true;
  if (!window.mermaid) {
    console.warn('[录音转总结] mermaid 未加载,思维导图将降级为文本大纲');
    return false;
  }
  window.mermaid.initialize({
    startOnLoad: false,
    theme: 'dark',
    securityLevel: 'loose',
    fontFamily: 'inherit',
    themeVariables: {
      background: '#101216',
      primaryColor: '#22262e',
      primaryTextColor: '#e6e9ef',
      primaryBorderColor: '#4a90d9',
      lineColor: '#4a5568',
      fontSize: '14px',
    },
  });
  mermaidReady = true;
  return true;
}

/** 渲染一张 Mermaid 图。失败返回 null,由调用方降级到文本大纲。 */
async function renderMermaid(code, target) {
  if (!initMermaid()) return null;
  try {
    await window.mermaid.parse(code);
  } catch (e) {
    console.warn('[录音转总结] mermaid 语法校验失败:', e);
    return null;
  }
  try {
    const { svg } = await window.mermaid.render('mm-' + Date.now(), code);
    target.innerHTML = svg;
    return target.querySelector('svg');
  } catch (e) {
    console.warn('[录音转总结] mermaid 渲染失败:', e);
    return null;
  }
}

function applyZoom() {
  const inner = $('mindmapTarget');
  if (inner) inner.style.transform = `scale(${MINDMAP.zoom})`;
  const st = $('mmStatus');
  if (st && st.dataset.mode !== 'error') {
    st.textContent = `缩放 ${Math.round(MINDMAP.zoom * 100)}%`;
  }
}

async function loadProjects() {
  const box = $('projectList');
  box.innerHTML = '<div class="empty">加载中…</div>';
  try {
    const rows = await invoke('list_projects');
    if (!rows.length) {
      box.innerHTML =
        '<div class="empty">还没有工程。<br><br>到「处理录音」拖入一个音频文件,处理完就会自动建工程。</div>';
      return;
    }
    box.innerHTML = rows
      .map((p) => {
        const arts = [
          ['录音', true],
          ['转写', !p.missing.includes('转写')],
          ['详细', !p.missing.includes('详细总结')],
          ['简略', !p.missing.includes('简略总结')],
          ['导图', p.has_mindmap],
        ]
          .map(([label, on]) => `<span class="art ${on ? 'on' : 'off'}">${on ? '✓' : '✗'} ${label}</span>`)
          .join('');
        return `
        <div class="item" data-id="${p.id}">
          <div class="item-main">
            <div class="item-title">${escapeHtml(p.title)}</div>
            <div class="item-sub">
              ${p.short_id} · ${p.duration_text} · ${fmtTime(p.created_at)}
              ${p.scene_label ? ' · ' + p.scene_label : ''}
              ${p.speaker_count ? ' · ' + p.speaker_count + ' 位发言人' : ''}
            </div>
            <div class="artifacts" style="margin-top:6px">${arts}</div>
          </div>
          <div class="item-right">
            ${p.complete ? '<span class="chip ok">完整</span>' : '<span class="chip warn">不完整</span>'}
          </div>
        </div>`;
      })
      .join('');
    box.querySelectorAll('.item').forEach((el) =>
      el.addEventListener('click', () => openProject(el.dataset.id))
    );
  } catch (e) {
    box.innerHTML = `<div class="empty">加载失败:${escapeHtml(String(e))}</div>`;
  }
}

let currentProject = null;

async function openProject(id) {
  window.__openCalls = (window.__openCalls || 0) + 1;
  try {
    const p = await invoke('get_project', { id });
    currentProject = p;

    $('projTitle').textContent = p.title;
    const bits = [p.id.slice(0, 8), p.duration_text, fmtTime(p.created_at)];
    if (p.scene_label) {
      bits.push('场景:' + p.scene_label);
      if (p.scene_confidence) bits.push(Math.round(p.scene_confidence * 100) + '%');
    }
    if (p.asr_model) bits.push(p.asr_model);
    if (p.asr_backend) bits.push(p.asr_backend.toUpperCase());
    if (p.speaker_count) bits.push(p.speaker_count + ' 位发言人');
    $('projMeta').textContent = bits.join(' · ');

    $('projBrief').innerHTML = renderMd(p.summary_brief);
    $('projDetailed').innerHTML = renderMd(p.summary_detailed);
    $('projTranscript').textContent = p.transcript || '(无转写)';
    $('projSrt').textContent = p.srt || '(无字幕)';
    $('mmOutline').textContent = p.outline || '(无大纲)';
    $('mmSource').textContent = p.mindmap || '(无导图源码)';
    MINDMAP.source = p.mindmap || '';
    MINDMAP.outline = p.outline || '';
    MINDMAP.zoom = 1;

    const banner = $('projBanner');
    const msgs = [];
    if (p.missing && p.missing.length) {
      msgs.push(
        `这个工程还缺:${p.missing.join('、')}。重新处理同一音频即可补齐(转写会命中缓存)。`
      );
    }
    if (p.mindmap && !p.mindmap_syntax_ok) {
      msgs.push(
        `思维导图语法自检未通过(${(p.mindmap_issues || []).join('、')}),可能渲染失败。`
      );
    }
    if (msgs.length) {
      banner.innerHTML = msgs.join('<br>');
      show(banner);
    } else {
      hide(banner);
    }

    // 导图只在切到那个标签页时才渲染,避免无谓开销
    $('mindmapTarget').innerHTML = '';
    $('mindmapTarget').dataset.rendered = '';
    $('mindmapWrap').classList.remove('hidden');
    $('mmSource').classList.add('hidden');
    hide($('mmFallback'));

    selectProjectTab('brief');
    document.querySelectorAll('.view').forEach((x) => x.classList.remove('active'));
    $('view-project').classList.add('active');
  } catch (e) {
    alert('打开工程失败:' + e);
  }
}

async function ensureMindmapRendered() {
  const target = $('mindmapTarget');
  if (target.dataset.rendered === '1') return;
  target.dataset.rendered = '1';

  const st = $('mmStatus');
  st.dataset.mode = '';

  if (!MINDMAP.source) {
    $('mmOutline').textContent = '(这个工程没有思维导图)';
    show($('mmFallback'));
    st.textContent = '';
    return;
  }

  // mermaid 没加载成功时要说清楚 —— 否则界面只是一片空白,用户不知道发生了什么
  if (!window.mermaid) {
    st.dataset.mode = 'error';
    st.textContent = '未加载 mermaid 库(ui/vendor/mermaid.min.js),已降级为文本大纲';
    $('mindmapWrap').classList.add('hidden');
    show($('mmFallback'));
    return;
  }

  st.textContent = '渲染中…';

  let svg = null;
  let errText = '';
  try {
    svg = await renderMermaid(MINDMAP.source, target);
  } catch (e) {
    errText = String((e && e.message) || e);
  }

  if (svg) {
    hide($('mmFallback'));
    $('mindmapWrap').classList.remove('hidden');
    st.textContent = '渲染成功';
    applyZoom();
  } else {
    // ★ 降级:图渲染不出来也要让用户有东西看,并且说明原因
    st.dataset.mode = 'error';
    st.textContent = errText
      ? `渲染失败:${errText}`
      : '渲染失败(语法不被 mermaid 接受),已降级为文本大纲';
    $('mindmapWrap').classList.add('hidden');
    show($('mmFallback'));
  }
}

function selectProjectTab(name) {
  window.__tabCalls = (window.__tabCalls || 0) + 1;
  window.__lastTab = name;
  document.querySelectorAll('#view-project .tab').forEach((t) =>
    t.classList.toggle('active', t.dataset.ptab === name)
  );
  document.querySelectorAll('#view-project .tabpane').forEach((p) =>
    p.classList.toggle('active', p.id === 'ppane-' + name)
  );
  if (name === 'mindmap') ensureMindmapRendered();
  if (name === 'files' && currentProject) loadProjectFiles(currentProject.id);
}

document.querySelectorAll('#view-project .tab').forEach((t) =>
  t.addEventListener('click', () => selectProjectTab(t.dataset.ptab))
);

async function loadProjectFiles(id) {
  const box = $('projFiles');
  box.innerHTML = '<div class="empty">加载中…</div>';
  try {
    const files = await invoke('list_project_files', { id });
    box.innerHTML = files
      .map(
        (f) => `
      <div class="file-row ${f.is_audio ? 'audio' : ''}">
        <span class="file-name">${escapeHtml(f.rel)}</span>
        <span class="file-size">${fmtBytes(f.size)}</span>
      </div>`
      )
      .join('');
  } catch (e) {
    box.innerHTML = `<div class="empty">加载失败:${escapeHtml(String(e))}</div>`;
  }
}

$('btnProjectBack').addEventListener('click', () => {
  loadProjects();
  gotoView('projects');
});

$('btnOpenDir').addEventListener('click', async () => {
  if (!currentProject) return;
  try {
    const dir = await invoke('open_project_dir', { id: currentProject.id });
    const t = window.__TAURI__;
    const op = t && t.opener;

    // 优先「在文件管理器中打开」。
    //
    // ⚠️ 这两个命令需要 capabilities 里显式授权:
    //    opener:allow-reveal-item-in-dir / opener:allow-open-path
    //    插件的 `opener:default` 是**空集**,不给任何命令 —— 之前只写了
    //    `opener:default`,于是 openPath 被拒,点了没反应。
    if (op && op.revealItemInDir) {
      await op.revealItemInDir(dir);
      return;
    }
    if (op && op.openPath) {
      await op.openPath(dir);
      return;
    }
    // 插件不可用时至少把路径给出来,别让用户什么都拿不到
    await copyToClipboard(dir);
    alert(
      '已复制工程目录路径(系统打开接口不可用):\n\n' +
        dir +
        '\n\n可直接粘到资源管理器地址栏。'
    );
  } catch (e) {
    // 失败时也把路径显示出来 —— 用户至少能自己去找
    const fallback = currentProject ? currentProject.dir : '';
    if (fallback) await copyToClipboard(fallback);
    alert(
      '打开目录失败:' +
        e +
        (fallback ? '\n\n路径已复制到剪贴板:\n' + fallback : '')
    );
  }
});

/** 复制到剪贴板;失败时静默(alert 里已经给了路径)。 */
async function copyToClipboard(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

/** 重命名工程。
 *
 *  **标题与目录名一起改** —— 只改一个会让界面显示的名字和资源管理器里
 *  看到的目录长期不一致,反而更难找。
 *
 *  改目录名会让云端路径全变,所以成功后要明确提醒用户。
 */
$('btnRenameProject').addEventListener('click', async () => {
  if (!currentProject) return;
  const oldTitle = currentProject.title;
  const input = prompt(
    '新名字:\n\n' +
      '(标题和文件夹名会一起改。日期前缀会保留。)\n' +
      '(注意:如果这个工程已经同步过,云端路径会跟着变。)',
    oldTitle
  );
  if (input === null) return; // 取消
  const title = input.trim();
  if (!title) {
    alert('名字不能为空。');
    return;
  }
  if (title === oldTitle) return; // 没变,不折腾

  try {
    const r = await invoke('rename_project', {
      id: currentProject.id,
      title,
    });

    // 重新拉一次详情,让界面显示新的标题与目录
    await openProject(currentProject.id);

    if (r.slug_changed) {
      alert(
        '已重命名。\n\n' +
          '标题:' +
          r.old_title +
          ' → ' +
          r.new_title +
          '\n' +
          '目录:' +
          r.old_slug +
          '\n   → ' +
          r.dir.split(/[\\/]/).pop() +
          '\n\n' +
          '⚠ 目录名变了,云端路径也会变。\n' +
          '如果这个工程已经同步过,云端旧路径下的文件会变成孤儿 ——\n' +
          '下次同步会上传新路径;旧路径需要手动清理。'
      );
    }
  } catch (e) {
    alert('改名失败:' + e);
  }
});

// --- 思维导图工具条 ---

$('btnMmZoomIn').addEventListener('click', () => {
  MINDMAP.zoom = Math.min(MINDMAP.zoom * 1.2, 4);
  applyZoom();
});
$('btnMmZoomOut').addEventListener('click', () => {
  MINDMAP.zoom = Math.max(MINDMAP.zoom / 1.2, 0.2);
  applyZoom();
});
$('btnMmFit').addEventListener('click', () => {
  const wrap = $('mindmapWrap');
  const svg = $('mindmapTarget').querySelector('svg');
  if (!svg || !wrap) return;
  const natural = svg.getBoundingClientRect().width / MINDMAP.zoom;
  const avail = wrap.clientWidth - 40;
  MINDMAP.zoom = natural > 0 ? Math.min(avail / natural, 1) : 1;
  applyZoom();
});
$('btnMmSource').addEventListener('click', () => {
  $('mmSource').classList.toggle('hidden');
});

/** 导出 PNG。
 *
 *  做法:把渲染出的 SVG 画进 canvas,再 toDataURL 拿 PNG,交给 Rust 写盘。
 *  这样不需要任何图像处理库 —— 浏览器本来就会渲染 SVG。
 */
$('btnMmPng').addEventListener('click', async () => {
  const svg = $('mindmapTarget').querySelector('svg');
  if (!svg) {
    alert('还没有渲染出图,无法导出。');
    return;
  }
  try {
    const path = await saveDialog({
      defaultPath: `${currentProject ? currentProject.title : 'mindmap'}-思维导图.png`,
      filters: [{ name: 'PNG', extensions: ['png'] }],
    });
    if (!path) return;
    const png = await svgToPngDataUrl(svg);
    await invoke('save_png', { path, dataUrl: png });
    alert('已导出到\n' + path);
  } catch (e) {
    alert('导出失败:' + e);
  }
});

/** SVG → PNG data URL。 */
async function svgToPngDataUrl(svg, scale = 2) {
  const vb = svg.viewBox && svg.viewBox.baseVal;
  const rect = svg.getBoundingClientRect();
  const w = (vb && vb.width) || rect.width || 800;
  const h = (vb && vb.height) || rect.height || 600;

  // 克隆一份并固定尺寸,避免把界面上的缩放带进导出的图
  const clone = svg.cloneNode(true);
  clone.setAttribute('width', w);
  clone.setAttribute('height', h);
  if (!clone.getAttribute('xmlns')) {
    clone.setAttribute('xmlns', 'http://www.w3.org/2000/svg');
  }

  const xml = new XMLSerializer().serializeToString(clone);
  const blob = new Blob([xml], { type: 'image/svg+xml;charset=utf-8' });
  const url = URL.createObjectURL(blob);

  try {
    const img = await new Promise((res, rej) => {
      const i = new Image();
      i.onload = () => res(i);
      i.onerror = () => rej(new Error('SVG 转图片失败'));
      i.src = url;
    });
    const canvas = document.createElement('canvas');
    canvas.width = Math.max(1, Math.round(w * scale));
    canvas.height = Math.max(1, Math.round(h * scale));
    const ctx = canvas.getContext('2d');
    // 深色底,和界面一致
    ctx.fillStyle = '#101216';
    ctx.fillRect(0, 0, canvas.width, canvas.height);
    ctx.drawImage(img, 0, 0, canvas.width, canvas.height);
    return canvas.toDataURL('image/png');
  } finally {
    URL.revokeObjectURL(url);
  }
}

// --- 复制 / 另存 ---

function projectTabContent() {
  const p = currentProject;
  if (!p) return '';
  const t = document.querySelector('#view-project .tab.active');
  switch (t ? t.dataset.ptab : 'brief') {
    case 'brief': return p.summary_brief || '';
    case 'detailed': return p.summary_detailed || '';
    case 'mindmap': return p.mindmap || '';
    case 'transcript': return p.transcript || '';
    case 'srt': return p.srt || '';
    default: return '';
  }
}

$('btnProjCopy').addEventListener('click', async () => {
  const c = projectTabContent();
  if (!c) return;
  await navigator.clipboard.writeText(c);
  const b = $('btnProjCopy');
  const old = b.textContent;
  b.textContent = '已复制 ✓';
  setTimeout(() => (b.textContent = old), 1400);
});

$('btnProjSave').addEventListener('click', async () => {
  const c = projectTabContent();
  if (!c) return;
  const t = document.querySelector('#view-project .tab.active');
  const tab = t ? t.dataset.ptab : 'brief';
  const ext = tab === 'srt' ? 'srt' : tab === 'mindmap' ? 'mmd' : 'md';
  const path = await saveDialog({
    defaultPath: `${currentProject.title}-${tab}.${ext}`,
    filters: [{ name: ext.toUpperCase(), extensions: [ext] }],
  });
  if (!path) return;
  try {
    await invoke('save_text', { path, content: c });
    alert('已保存到\n' + path);
  } catch (e) {
    alert('保存失败:' + e);
  }
});

// ---------------------------------------------------------------------------
// 历史
// ---------------------------------------------------------------------------

async function loadHistory() {
  const box = $('historyList');
  box.innerHTML = '<div class="empty">加载中…</div>';
  try {
    const rows = await invoke('list_sessions', { limit: 200 });
    if (!rows.length) {
      box.innerHTML =
        '<div class="empty">还没有处理过的录音。<br><br>到「处理录音」拖入一个音频文件开始。</div>';
      return;
    }
    box.innerHTML = rows
      .map(
        (r) => `
      <div class="item" data-id="${r.id}">
        <div class="item-main">
          <div class="item-title">${escapeHtml(r.title)}</div>
          <div class="item-sub">
            ${r.short_id} · ${r.duration_text} · ${fmtTime(r.created_at)}
            ${r.scene_label ? ' · ' + r.scene_label : ''}
            ${r.speaker_count ? ' · ' + r.speaker_count + ' 位发言人' : ''}
          </div>
        </div>
        <div class="item-right">
          ${r.has_summary ? '<span class="chip ok">有纪要</span>' : '<span class="chip">仅转写</span>'}
        </div>
      </div>`
      )
      .join('');
    box.querySelectorAll('.item').forEach((el) =>
      el.addEventListener('click', () => openSession(el.dataset.id))
    );
  } catch (e) {
    box.innerHTML = `<div class="empty">加载失败:${e}</div>`;
  }
}

function escapeHtml(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

// ---------------------------------------------------------------------------
// 详情
// ---------------------------------------------------------------------------

async function openSession(id) {
  try {
    const d = await invoke('get_session', { id });
    S.currentDetail = d;
    S.currentSession = d.id;

    $('detailTitle').textContent = d.title;
    const bits = [d.id.slice(0, 8), d.duration_text];
    if (d.scene_label) {
      bits.push('场景:' + d.scene_label);
      if (d.scene_confidence) bits.push(Math.round(d.scene_confidence * 100) + '%');
    }
    if (d.used_map_reduce) bits.push('长音频分段总结');
    $('detailMeta').textContent = bits.join(' · ');

    $('contentSummary').innerHTML = renderMd(d.summary);
    $('contentDialogue').textContent = d.transcript;
    $('contentTimeline').textContent = d.timeline;
    $('contentPlain').textContent = d.plain;
    $('contentSrt').textContent = d.srt;

    renderSpeakers(d);

    // 低置信度 / 过时提示
    const banner = $('staleBanner');
    if (d.summary_stale) {
      banner.innerHTML =
        '发言人名字已更新,当前纪要里仍是旧名字。重新处理同一音频即可生效(转写会命中缓存,只需几秒)。';
      show(banner);
    } else if (d.scene_low_confidence) {
      banner.innerHTML = `场景判断不太确定(已按「${d.scene_label}」生成)。可在设置里更换模型后重新处理。`;
      show(banner);
    } else {
      hide(banner);
    }

    selectTab('summary');
    gotoView('history');
    document.querySelectorAll('.view').forEach((x) => x.classList.remove('active'));
    $('view-detail').classList.add('active');
  } catch (e) {
    alert('打开失败:' + e);
  }
}

function renderSpeakers(d) {
  const box = $('contentSpeakers');
  if (!d.labels || !d.labels.length) {
    box.innerHTML =
      '<div class="empty">这个会话没有发言人信息。<br><br>可能是处理时跳过了说话人区分,或声纹模型未就位。</div>';
    return;
  }
  box.innerHTML = d.labels
    .map(
      (s) => `
    <div class="spk">
      <span class="spk-dot" style="background:${s.color}"></span>
      <input class="spk-name" data-sid="${s.id}" value="${escapeHtml(s.name)}">
      <span class="spk-meta">${s.talk_time_text} · ${s.segment_count} 段${
        s.profile_id ? ' · 已建档' : ''
      }</span>
    </div>`
    )
    .join('');

  box.querySelectorAll('.spk-name').forEach((inp) => {
    inp.addEventListener('change', async () => {
      const sid = parseInt(inp.dataset.sid);
      try {
        const nd = await invoke('rename_speaker', {
          id: S.currentSession,
          speakerId: sid,
          name: inp.value,
        });
        S.currentDetail = nd;
        $('contentDialogue').textContent = nd.transcript;
        $('contentTimeline').textContent = nd.timeline;
        $('contentPlain').textContent = nd.plain;
        $('contentSrt').textContent = nd.srt;
        // ★ 改名只动标签,不重新转写 —— 所以这里是即时的
        logTo($('progLog'), `已把 [${sid}] 改名为「${inp.value}」`, 'ln-ok');
      } catch (e) {
        alert('改名失败:' + e);
      }
    });
  });
}

$('btnBack').addEventListener('click', () => gotoView('history'));

// ⚠️ 选择器必须限定在 #view-detail 内。
//
// 这里曾经用裸的 `querySelectorAll('.tab')` / `('.tabpane')`,结果是:
// 点工程详情(#view-project)的标签页时,两个处理器都会触发 ——
// 工程的处理器刚把 active 加到 ppane-mindmap,旧的处理器立刻把所有 .tabpane
// 的 active 全清掉(因为 ppane-mindmap !== pane-undefined),界面变成一片空白。
function selectTab(name) {
  document.querySelectorAll('#view-detail .tab').forEach((t) =>
    t.classList.toggle('active', t.dataset.tab === name)
  );
  document.querySelectorAll('#view-detail .tabpane').forEach((p) =>
    p.classList.toggle('active', p.id === 'pane-' + name)
  );
}
document.querySelectorAll('#view-detail .tab').forEach((t) =>
  t.addEventListener('click', () => selectTab(t.dataset.tab))
);

function currentTabName() {
  const t = document.querySelector('#view-detail .tab.active');
  return t ? t.dataset.tab : 'summary';
}

function currentTabContent() {
  const d = S.currentDetail;
  if (!d) return '';
  switch (currentTabName()) {
    case 'summary': return d.summary || '';
    case 'dialogue': return d.transcript || '';
    case 'timeline': return d.timeline || '';
    case 'plain': return d.plain || '';
    case 'srt': return d.srt || '';
    default: return '';
  }
}

$('btnCopyTab').addEventListener('click', async () => {
  const c = currentTabContent();
  if (!c) return;
  await navigator.clipboard.writeText(c);
  const b = $('btnCopyTab');
  const old = b.textContent;
  b.textContent = '已复制 ✓';
  setTimeout(() => (b.textContent = old), 1400);
});

$('btnSaveTab').addEventListener('click', async () => {
  const c = currentTabContent();
  if (!c) return;
  const tab = currentTabName();
  const ext = tab === 'srt' ? 'srt' : tab === 'summary' ? 'md' : 'txt';
  const path = await saveDialog({
    defaultPath: `${S.currentSession.slice(0, 8)}-${tab}.${ext}`,
    filters: [{ name: ext.toUpperCase(), extensions: [ext] }],
  });
  if (!path) return;
  try {
    await invoke('save_text', { path, content: c });
    alert('已保存到\n' + path);
  } catch (e) {
    alert('保存失败:' + e);
  }
});

$('btnRegenHint').addEventListener('click', () => {
  alert(
    '重新生成纪要的步骤:\n\n' +
    '1. 到「处理录音」\n' +
    '2. 拖入同一个音频文件\n' +
    '3. 开始处理\n\n' +
    '转写结果会命中缓存(不会重新转写),\n' +
    '所以只需要几秒就能用新名字生成纪要。'
  );
});

// ---------------------------------------------------------------------------
// 声纹档案
// ---------------------------------------------------------------------------

async function loadProfiles() {
  const box = $('profileList');
  box.innerHTML = '<div class="empty">加载中…</div>';
  try {
    const rows = await invoke('list_profiles');
    if (!rows.length) {
      box.innerHTML =
        '<div class="empty">还没有声纹档案。<br><br>' +
        '档案会在你给发言人改名字时自动建立 —— <b>改名即登记</b>。<br>' +
        '这样下次录到同一个人时会自动识别并标注。</div>';
      return;
    }
    box.innerHTML = rows
      .map(
        (p) => `
      <div class="item">
        <div class="item-main">
          <div class="item-title">${escapeHtml(p.name)}</div>
          <div class="item-sub">${p.sample_count} 条样本 · 共 ${p.total_minutes} 分钟 · ${p.model}</div>
        </div>
        <div class="item-right">
          <button class="btn ghost sm" data-act="rename" data-id="${p.id}" data-name="${escapeHtml(p.name)}">改名</button>
          <button class="btn ghost sm" data-act="del" data-id="${p.id}">删除</button>
        </div>
      </div>`
      )
      .join('');

    box.querySelectorAll('button[data-act]').forEach((b) =>
      b.addEventListener('click', async (e) => {
        e.stopPropagation();
        const id = b.dataset.id;
        if (b.dataset.act === 'del') {
          if (!confirm('删除这个档案?包含全部登记样本,不可恢复。')) return;
          await invoke('delete_profile', { profileId: id });
        } else {
          const n = prompt('新的名字:', b.dataset.name);
          if (!n) return;
          await invoke('rename_profile', { profileId: id, name: n });
        }
        loadProfiles();
      })
    );
  } catch (e) {
    box.innerHTML = `<div class="empty">加载失败:${e}</div>`;
  }
}

// ---------------------------------------------------------------------------
// 同步
// ---------------------------------------------------------------------------

async function loadSync() {
  try {
    const s = await invoke('get_sync_config');
    $('syncUrl').value = s.url;
    $('syncUser').value = s.username;
    $('syncDir').value = s.remote_dir;
    $('syncPass').placeholder = s.has_password ? '已保存(留空则不修改)' : '尚未设置';
    const hint = await invoke('audio_sync_hint');
    $('audioHint').textContent = hint;
  } catch (e) {
    logTo($('syncLog'), '加载配置失败:' + e, 'ln-warn');
  }
  // 范围与清单:首屏不联网(probe=false),秒开
  await loadScope();
  await loadInventory(false);
}

// ---------------------------------------------------------------------------
// 同步范围(方向 / 删除 / 录音)
// ---------------------------------------------------------------------------

/** 单选组的视觉同步。
 *
 *  CSS 的 :has() 在旧 WebView2 上不一定可用,所以用类名显式控制。
 */
function paintRadios(groupId) {
  const box = $(groupId);
  if (!box) return;
  box.querySelectorAll('.radio').forEach((l) => {
    const inp = l.querySelector('input');
    l.classList.toggle('checked', !!(inp && inp.checked));
  });
}

function radioValue(name) {
  const el = document.querySelector(`input[name="${name}"]:checked`);
  return el ? el.value : null;
}

function setRadio(name, value) {
  const el = document.querySelector(`input[name="${name}"][value="${value}"]`);
  if (el) el.checked = true;
}

async function loadScope() {
  try {
    const s = await invoke('get_sync_selection');
    setRadio('dir', s.direction);
    setRadio('del', s.deletion);
    setRadio('aud', s.audio);
    ['dirRadios', 'delRadios', 'audioRadios'].forEach(paintRadios);
    $('scopeSummary').textContent =
      s.describe + (s.user_excludes.length ? `(${s.user_excludes.length} 项被取消勾选)` : '');
  } catch (e) {
    $('scopeSummary').textContent = '加载失败:' + e;
  }
}

['dirRadios', 'delRadios', 'audioRadios'].forEach((id) => {
  const box = $(id);
  if (box) box.addEventListener('change', () => paintRadios(id));
});

$('btnScopeSave').addEventListener('click', async () => {
  try {
    const s = await invoke('set_sync_selection', {
      direction: radioValue('dir'),
      audio: radioValue('aud'),
      deletion: radioValue('del'),
    });
    $('scopeSummary').textContent = s.describe;
    // 方向/策略变了,清单状态会变 —— 刷新
    await loadInventory(false);
  } catch (e) {
    alert('保存失败:' + e);
  }
});

// ---------------------------------------------------------------------------
// 同步清单树
// ---------------------------------------------------------------------------

const INV = { tree: null, collapsed: new Set() };

/** 状态 → 徽章样式。 */
function badgeClass(status) {
  switch (status) {
    case 'synced': return 'ok';
    case 'pending-upload':
    case 'pending-download':
    case 'pending-delete':
    case 'conflict': return 'warn';
    default: return 'muted';
  }
}

/** 状态 → 中文标签 + 符号。 */
const STATUS_TEXT = {
  synced: ['✓', '已同步'],
  'pending-upload': ['↑', '待上传'],
  'pending-download': ['↓', '待下载'],
  'pending-delete': ['✗', '待删除云端'],
  excluded: ['—', '未同步'],
  'never-synced': ['·', '从未同步'],
  'deleted-local-kept-remote': ['◌', '本地已删(云端保留)'],
  conflict: ['!', '需处理'],
};

function statusText(s) {
  const [g, t] = STATUS_TEXT[s] || ['?', s];
  return `${g} ${t}`;
}

async function loadInventory(probe) {
  const box = $('invTree');
  if (probe) box.innerHTML = '<div class="empty">正在核实云端状态…(文件多时需要一会儿)</div>';
  else box.innerHTML = '<div class="empty">加载中…</div>';

  try {
    const tree = await invoke('get_sync_inventory', { probe: !!probe });
    INV.tree = tree;
    renderLegend();
    renderTree();
    if (probe) {
      // 核实完刷新一下范围摘要(排除数可能没变,但状态变了)
      await loadScope();
    }
  } catch (e) {
    box.innerHTML = `<div class="empty">加载失败:${escapeHtml(String(e))}</div>`;
  }
}

function renderLegend() {
  $('invLegend').innerHTML = [
    ['ok', '✓ 已同步'],
    ['warn', '↑ 待上传'],
    ['warn', '↓ 待下载'],
    ['warn', '✗ 待删除云端'],
    ['muted', '— 未同步(已取消勾选)'],
    ['muted', '◌ 本地已删(云端保留)'],
  ]
    .map(([c, t]) => `<span class="inv-badge ${c}">${t}</span>`)
    .join('');
}

/** 递归渲染一个节点及其子树。 */
function renderNode(node, depth, out) {
  const hasKids = node.children && node.children.length > 0;
  const key = node.rel_path || '__root__' + depth;
  const isCollapsed = INV.collapsed.has(key);

  const caret = hasKids ? (isCollapsed ? '▸' : '▾') : '';
  const cls = [
    'inv-row',
    node.is_dir ? 'dir' : 'file',
    node.in_scope ? '' : 'excluded',
  ]
    .filter(Boolean)
    .join(' ');

  // 复选框:文件的勾选状态就是 in_scope;文件夹同理由父级覆盖体现
  const checked = node.in_scope ? 'checked' : '';
  // 根节点是虚拟的,不给复选框
  const isVirtualRoot = node.rel_path === '' && depth === 0;
  const checkbox = isVirtualRoot
    ? '<span class="inv-check" style="width:13px"></span>'
    : `<input type="checkbox" class="inv-check" data-path="${escapeHtml(node.rel_path)}" ${
        checked
      }>`;

  const sizeTxt = node.size != null && !node.is_dir ? fmtBytes(node.size) : '';
  const badge = `<span class="inv-badge ${badgeClass(node.status)}">${statusText(
    node.status
  )}</span>`;

  out.push(
    `<div class="${cls}" data-key="${escapeHtml(key)}" data-path="${escapeHtml(
      node.rel_path
    )}" data-dir="${node.is_dir ? '1' : '0'}" style="padding-left:${
      10 + depth * 16
    }px">
      <span class="inv-caret">${caret}</span>
      ${checkbox}
      <span class="inv-name">${escapeHtml(node.name)}</span>
      ${node.is_audio ? '<span class="inv-badge muted">音频</span>' : ''}
      <span class="inv-size">${sizeTxt}</span>
      ${badge}
    </div>`
  );

  if (hasKids) {
    out.push(`<div class="inv-kids ${isCollapsed ? 'collapsed' : ''}" data-kids="${escapeHtml(key)}">`);
    for (const c of node.children) renderNode(c, depth + 1, out);
    out.push('</div>');
  }
}

function renderTree() {
  if (!INV.tree) return;
  const out = [];
  // 跳过虚拟根,直接从工程开始列(少一层无意义的缩进)
  for (const c of INV.tree.children) renderNode(c, 0, out);
  if (!out.length) {
    $('invTree').innerHTML =
      '<div class="empty">还没有可同步的内容。<br><br>处理一段录音后会生成工程。</div>';
    return;
  }
  $('invTree').innerHTML = out.join('');

  // 折叠加展开
  $('invTree').querySelectorAll('.inv-row.dir').forEach((row) => {
    row.addEventListener('click', (ev) => {
      // 点复选框不算折叠
      if (ev.target.classList.contains('inv-check')) return;
      const key = row.dataset.key;
      if (INV.collapsed.has(key)) INV.collapsed.delete(key);
      else INV.collapsed.add(key);
      renderTree();
    });
  });

  // 勾选
  $('invTree').querySelectorAll('input.inv-check[data-path]').forEach((cb) => {
    cb.addEventListener('change', async () => {
      const p = cb.dataset.path;
      cb.disabled = true;
      try {
        await invoke('set_sync_excluded', { path: p, excluded: !cb.checked });
        await loadScope();
        await loadInventory(false);
      } catch (e) {
        alert('修改失败:' + e);
        cb.checked = !cb.checked;
      } finally {
        cb.disabled = false;
      }
    });
  });
}

$('btnInvExpand').addEventListener('click', () => {
  INV.collapsed.clear();
  renderTree();
});

$('btnInvCollapse').addEventListener('click', () => {
  if (!INV.tree) return;
  // 折叠所有有子节点的目录
  const walk = (n, depth) => {
    if (n.children && n.children.length) {
      INV.collapsed.add(n.rel_path || '__root__' + depth);
      n.children.forEach((c) => walk(c, depth + 1));
    }
  };
  INV.tree.children.forEach((c) => walk(c, 0));
  renderTree();
});

$('btnInvProbe').addEventListener('click', async () => {
  const btn = $('btnInvProbe');
  btn.disabled = true;
  try {
    await loadInventory(true);
  } finally {
    btn.disabled = false;
  }
});

$('btnInvClear').addEventListener('click', async () => {
  if (!confirm('恢复全部同步?这会清空所有「取消勾选」的记录。\n\n不会删除任何文件,只是让它们重新参与同步。')) {
    return;
  }
  try {
    await invoke('clear_sync_excludes');
    INV.collapsed.clear();
    await loadScope();
    await loadInventory(false);
  } catch (e) {
    alert('操作失败:' + e);
  }
});

$('btnSyncSave').addEventListener('click', async () => {
  show($('syncLog'));
  try {
    const note = await invoke('set_sync_config', {
      url: $('syncUrl').value,
      username: $('syncUser').value,
      remoteDir: $('syncDir').value,
      password: $('syncPass').value || null,
    });
    $('syncPass').value = '';
    logTo($('syncLog'), '✓ 已保存(密码在 Windows 凭据管理器)', 'ln-ok');
    if (note) {
      // 地址里含子目录时程序会自动拆开 —— 要让用户知道,否则他会以为填错了
      logTo($('syncLog'), 'ℹ ' + note, 'ln-info');
      await loadSync(); // 重新拉一次,把拆开后的值显示出来
    }
  } catch (e) {
    logTo($('syncLog'), '✗ ' + e, 'ln-warn');
  }
});

/** 收集当前表单里的同步配置。
 *
 *  「测试连接」「查看计划」「开始同步」都用它 —— 这样不必先保存就能验证。
 *  用户填完直接点测试是自然行为,不该被"请先保存"挡住。
 */
function syncFormValues() {
  return {
    url: ($('syncUrl').value || '').trim(),
    username: ($('syncUser').value || '').trim(),
    remoteDir: ($('syncDir').value || '').trim(),
    password: $('syncPass').value || null,
  };
}

$('btnSyncTest').addEventListener('click', async () => {
  show($('syncLog'));
  const v = syncFormValues();
  logTo($('syncLog'), '正在测试连接…');

  // 把实际要用的值打出来 —— 出问题时一眼能看出是"没填"还是"填错了"
  logTo($('syncLog'), `  地址 ${v.url || '(空)'}  ·  用户 ${v.username || '(空)'}`, 'ln-info');

  try {
    const r = await invoke('test_sync', v);
    logTo($('syncLog'), '✓ ' + r, 'ln-ok');
  } catch (e) {
    logTo($('syncLog'), '✗ ' + e, 'ln-warn');
    if (!v.username) {
      logTo($('syncLog'), '  提示:「用户名」框是空的,请填写 <账号>@auth.local', 'ln-info');
    } else {
      logTo($('syncLog'), '  提示:测试用的是当前填写的内容,不必先保存。', 'ln-info');
    }
  }
});

$('btnSyncPlan').addEventListener('click', async () => {
  show($('planCard'));
  show($('syncLog'));
  const box = $('planList');
  box.innerHTML = '<div class="empty">正在比对…</div>';
  try {
    const plan = await invoke('sync_plan', syncFormValues());
    if (!plan.length) {
      box.innerHTML = '<div class="empty">没有需要同步的文本文件。</div>';
      return;
    }
    box.innerHTML =
      `<div class="hw-row"><span>文件</span><b>${plan.length} 个</b></div><hr style="border:0;border-top:1px solid var(--line);margin:8px 0">` +
      plan
        .map(
          (p) =>
            `<div class="hw-row"><span style="font-size:11px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:420px">${escapeHtml(
              p.path
            )}</span><b style="font-size:11px">${p.action}${
              p.size ? ' · ' + fmtBytes(p.size) : ''
            }</b></div>`
        )
        .join('');
  } catch (e) {
    box.innerHTML = `<div class="empty">比对失败:${e}</div>`;
  }
});

$('btnSyncRun').addEventListener('click', async () => {
  show($('syncLog'));
  const v = syncFormValues();
  if (!confirm('开始同步?\n\n建议先点「查看同步计划」确认要传什么、要删什么。')) {
    return;
  }
  logTo($('syncLog'), `开始同步…(用户 ${v.username || '(空)'})`);
  try {
    const r = await invoke('run_sync', v);
    logTo($('syncLog'), '✓ ' + r, 'ln-ok');
    await loadInventory(false);
  } catch (e) {
    logTo($('syncLog'), '✗ ' + e, 'ln-warn');
  }
});

listen('sync://progress', (ev) => {
  const p = ev.payload;
  $('progDetail') &&
    ($('progDetail').textContent = `[${p.done}/${p.total}] ${p.path}`);
});

// ---------------------------------------------------------------------------
// 云端管理
// ---------------------------------------------------------------------------

/** 云端树的折叠状态,按 rel_path 记。 */
const CLOUD = { collapsed: new Set(), mode: '' };

function cloudEsc(s) {
  return String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}

/** 渲染云端节点。`depth` 用于缩进。
 *
 *  ★ 整行都可点,不只是那个三角。
 *
 *  之前只有三角带 `data-cloud-toggle`,点行本身没反应 —— 用户点工程目录
 *  时以为"点不进去"。三角只有 0.75rem 宽,很难瞄准。
 *
 *  另外:已经拉到最深一层时 `children` 为空,但要给「展开」的机会 ——
 *  所以目录一律显示可展开的三角,展开时若还没有子节点就去懒加载。
 */
function cloudNodeHtml(n, depth, loaded) {
  const pad = 'padding-left:' + (0.375 + depth * 1.1) + 'rem';
  const isCollapsed = CLOUD.collapsed.has(n.rel_path);
  const kids = n.children || [];
  // loaded=false 表示"这一层还没拉过",展开时需要去云端取
  const canExpand = n.is_dir;

  let caret = '<span class="inv-caret"></span>';
  if (canExpand) {
    const sym = isCollapsed ? '▶' : kids.length ? '▼' : '▸';
    caret = `<span class="inv-caret">${sym}</span>`;
  }

  const icon = n.is_dir ? '📁' : '📄';
  const size = n.is_dir ? '' : `<span class="cloud-size">${fmtBytes(n.size || 0)}</span>`;
  const time = n.modified ? `<span class="cloud-time">${cloudEsc(fmtHttpDate(n.modified))}</span>` : '';
  const lazy = canExpand && loaded === false ? '1' : '0';

  return `
    <div class="cloud-row ${n.is_dir ? 'dir' : ''}" style="${pad}"
         ${n.is_dir ? `data-cloud-dir="${cloudEsc(n.rel_path)}" data-cloud-lazy="${lazy}"` : ''}>
      ${caret}
      <span class="cloud-name">${icon} ${cloudEsc(n.name)}</span>
      ${size}${time}
      <button class="cloud-del" data-cloud-del="${cloudEsc(n.rel_path)}" data-cloud-isdir="${n.is_dir ? 1 : 0}"
        title="${n.is_dir ? '删除这个目录及其全部内容' : '删除这个文件'}">删除</button>
    </div>`;
}

function cloudTreeHtml(nodes, depth) {
  let out = '';
  for (const n of nodes) {
    const hasKids = n.is_dir && (n.children || []).length > 0;
    out += cloudNodeHtml(n, depth, hasKids || !n.is_dir);
    if (hasKids) {
      const collapsed = CLOUD.collapsed.has(n.rel_path) ? 'collapsed' : '';
      out += `<div class="cloud-kids ${collapsed}" data-cloud-kids="${cloudEsc(n.rel_path)}">`;
      out += cloudTreeHtml(n.children, depth + 1);
      out += '</div>';
    }
  }
  return out;
}

/** 对照节点:`attention > 0` 的展开显示,其余折叠。 */
function cloudDiffHtml(nodes, depth) {
  let out = '';
  for (const n of nodes) {
    const pad = 'padding-left:' + (0.375 + depth * 1.1) + 'rem';
    const kids = n.children || [];
    const isCollapsed = CLOUD.collapsed.has('D:' + n.rel_path);

    let caret = '<span class="inv-caret"></span>';
    if (n.is_dir && kids.length) {
      caret = `<span class="inv-caret">${isCollapsed ? '▶' : '▼'}</span>`;
    }

    // 徽章:一眼看出这个文件/目录是什么状态
    let badge = '';
    if (n.is_dir) {
      if (n.attention === 0) {
        badge = '<span class="cloud-badge same">一致</span>';
      } else {
        const bits = [];
        if (n.differing) bits.push(`不同 ${n.differing}`);
        if (n.local_only) bits.push(`仅本地 ${n.local_only}`);
        if (n.remote_only) bits.push(`仅云端 ${n.remote_only}`);
        badge = `<span class="cloud-badge differ">${bits.join(' · ')}</span>`;
      }
    } else if (n.status === 'same') {
      badge = '<span class="cloud-badge same">一致</span>';
    } else if (n.status === 'differ') {
      badge = '<span class="cloud-badge differ">大小不同</span>';
    } else if (n.status === 'local_only') {
      badge = '<span class="cloud-badge local">仅本地</span>';
    } else {
      badge = '<span class="cloud-badge remote">仅云端</span>';
    }

    const ls = n.local_size != null ? fmtBytes(n.local_size) : '—';
    const rs = n.remote_size != null ? fmtBytes(n.remote_size) : '—';
    const sizeTxt = n.is_dir ? '' : `<span class="cloud-size">本地 ${ls} / 云端 ${rs}</span>`;

    // ★ 单文件同步按钮。
    //
    // 只在**两边状态不一致**时出现 —— 一致的没什么可同步的,给按钮只是噪音。
    const syncBtns = n.is_dir ? '' : syncButtonsHtml(n);

    // 只有云端存在的条目才给删除按钮 —— 本地也有的删了会立刻被同步传回来
    const canDelete = !n.is_dir ? n.status !== 'local_only' : n.remote_only > 0 || n.attention > 0;
    const del = canDelete
      ? `<button class="cloud-del" data-cloud-del="${cloudEsc(n.rel_path)}" data-cloud-isdir="${n.is_dir ? 1 : 0}"
           title="${n.is_dir ? '删除云端这一支' : '删除云端这个文件'}">删除</button>`
      : '';

    out += `
      <div class="cloud-row ${n.is_dir ? 'dir' : ''}" style="${pad}"
           ${n.is_dir ? `data-cloud-dir="D:${cloudEsc(n.rel_path)}" data-cloud-lazy="0"` : ''}>
        ${caret}
        <span class="cloud-name">${n.is_dir ? '📁' : '📄'} ${cloudEsc(n.name)}</span>
        ${sizeTxt}
        ${badge}
        ${syncBtns}
        ${del}
      </div>`;

    if (n.is_dir && kids.length) {
      // 需要关注的目录默认展开,一致的收起 —— 用户想看的是有差异的部分
      const forceOpen = n.attention > 0;
      const collapsed =
        !forceOpen && isCollapsed ? 'collapsed' : forceOpen ? '' : CLOUD.collapsed.has('D:' + n.rel_path) ? 'collapsed' : '';
      out += `<div class="cloud-kids ${collapsed}" data-cloud-kids="D:${cloudEsc(n.rel_path)}">`;
      out += cloudDiffHtml(kids, depth + 1);
      out += '</div>';
    }
  }
  return out;
}

/** HTTP 日期 → 本地短格式。解析不了就原样返回。 */
function fmtHttpDate(s) {
  const d = new Date(s);
  if (isNaN(d.getTime())) return s;
  const p = (x) => String(x).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** 为"两边不一致"的文件生成同步按钮。
 *
 *  方向由用户决定 —— 这正是"与本地对照"的核心价值:
 *  看到差异,然后自己选哪一边赢。
 *
 *  但两个按钮平铺会让人犹豫,所以给出一个**建议**:
 *  大小不同时,较大的那份通常是较新的(编辑一般是在原稿上加东西)。
 *  这只是启发式而不是事实,所以按钮文字直说"建议",另一个也始终在。
 */
function syncButtonsHtml(n) {
  const rel = cloudEsc(n.rel_path);
  if (n.status === 'same') return '';

  const onlyLocal = n.status === 'local_only';
  const onlyRemote = n.status === 'remote_only';

  let suggest = '';
  if (onlyLocal) {
    suggest = 'up';
  } else if (onlyRemote) {
    suggest = 'down';
  } else {
    // 两边都有但大小不同:大的那份更新
    suggest = (n.local_size || 0) >= (n.remote_size || 0) ? 'up' : 'down';
  }

  const up = `<button class="cloud-sync up ${suggest === 'up' ? 'suggested' : ''}"
      data-cloud-sync="${rel}" data-cloud-way="upload"
      title="把本地这份传上去,覆盖云端">↑ 上传</button>`;

  const down = `<button class="cloud-sync down ${suggest === 'down' ? 'suggested' : ''}"
      data-cloud-sync="${rel}" data-cloud-way="download"
      title="把云端这份拉下来,覆盖本地">↓ 下载</button>`;

  // 只有一边有时,另一个方向没有意义 —— 只给能用的那个
  if (onlyLocal) return up;
  if (onlyRemote) return down;
  return up + down;
}

/** 云端树的展开/折叠与删除,用事件委托 —— 树是重绘的,逐个绑定会丢。
 *
 *  ★ 点**整行**都能展开,不是只有那个三角。
 *    三角只有 0.75rem 宽,瞄不准;而且用户直觉就是"点文件夹进去"。
 */
$('cloudTree').addEventListener('click', async (e) => {
  // 单文件同步 —— 优先级最高,它和删除都在行内
  const sb = e.target.closest('[data-cloud-sync]');
  if (sb) {
    await doSyncOne(sb);
    return;
  }

  // 删除按钮
  const del = e.target.closest('[data-cloud-del]');
  if (del) {
    const rel = del.dataset.cloudDel;
    const isDir = del.dataset.cloudIsdir === '1';

    // ★ 删除不可撤销,确认信息要说清楚删什么
    const msg = isDir
      ? `删除云端目录?\n\n${rel}\n\n⚠ 目录里的全部文件和子目录都会一起删掉,不可恢复。`
      : `删除云端文件?\n\n${rel}\n\n⚠ 不可恢复。`;
    if (!confirm(msg)) return;

    del.disabled = true;
    del.textContent = '删除中…';
    try {
      const r = await invoke('cloud_delete', { relPath: rel, isDir });
      let line = '已删除';
      if (r.files_deleted) line += ` ${r.files_deleted} 个文件`;
      if (r.dirs_deleted) line += `${r.files_deleted ? '、' : ' '}${r.dirs_deleted} 个目录`;
      if (r.bytes_deleted) line += `,共 ${fmtBytes(r.bytes_deleted)}`;
      if (r.failed && r.failed.length) {
        line += `\n\n有 ${r.failed.length} 项删不掉:\n` + r.failed.slice(0, 8).join('\n');
      }
      alert(line);
      // 重新拉一次,让树反映最新状态
      await reloadCloud();
    } catch (err) {
      alert('删除失败:' + err);
      del.disabled = false;
      del.textContent = '删除';
    }
    return;
  }

  // 展开/折叠:整行可点(文件行没有 data-cloud-dir,自然跳过)
  const row = e.target.closest('[data-cloud-dir]');
  if (!row) return;
  await toggleRow(row, row.dataset.cloudDir);
});

/** 展开或折叠一行。子节点还没拉过时去云端取。 */
async function toggleRow(row, key) {
  const tree = $('cloudTree');
  let kids = tree.querySelector(`[data-cloud-kids="${CSS.escape(key)}"]`);

  if (kids) {
    kids.classList.toggle('collapsed');
    const collapsed = kids.classList.contains('collapsed');
    if (collapsed) CLOUD.collapsed.add(key);
    else CLOUD.collapsed.delete(key);
    updateCaret(row, collapsed);
    return;
  }

  // 没有子节点容器 —— 说明这一层还没拉过(首屏只拉了有限深度)
  if (row.dataset.cloudLazy !== '1') {
    // 拉过了但没有子项 = 空目录,没什么可展开的
    return;
  }

  updateCaret(row, false, '…');
  const rel = key.startsWith('D:') ? key.slice(2) : key;
  try {
    // 拉下一层(不递归),插到这一行后面
    const nodes = await invoke('cloud_tree', { relDir: rel, depth: 1 });
    const holder = document.createElement('div');
    holder.className = 'cloud-kids';
    holder.dataset.cloudKids = key;
    holder.innerHTML = nodes.length
      ? cloudTreeHtml(nodes, indentOf(row) + 1)
      : '<div class="cloud-row" style="opacity:.6"><span class="cloud-name">(空目录)</span></div>';
    row.after(holder);
    row.dataset.cloudLazy = '0';
    CLOUD.collapsed.delete(key);
    updateCaret(row, false);
  } catch (err) {
    updateCaret(row, true);
    alert('读取这个目录失败:' + err);
  }
}

/** 从行的 padding-left 反推它处在第几层。 */
function indentOf(row) {
  const m = /padding-left:\s*([\d.]+)rem/.exec(row.getAttribute('style') || '');
  if (!m) return 0;
  return Math.max(0, Math.round((parseFloat(m[1]) - 0.375) / 1.1));
}

/** 更新行首的三角符号。 */
function updateCaret(row, collapsed, override) {
  const c = row.querySelector('.inv-caret');
  if (!c) return;
  c.textContent = override || (collapsed ? '▶' : '▼');
}

/** 同步单个文件。
 *
 *  ⚠️ 两个方向都会**覆盖**对面那一份,所以先确认。
 *  确认框里写清方向,免得点错 —— "上传"和"下载"在中文里很容易看反。
 */
async function doSyncOne(btn) {
  const rel = btn.dataset.cloudSync;
  const way = btn.dataset.cloudWay;
  const isUp = way === 'upload';

  const msg = isUp
    ? `把本地这份传上去?\n\n${rel}\n\n⚠ 云端的同名文件会被覆盖。`
    : `把云端这份拉下来?\n\n${rel}\n\n⚠ 本地的同名文件会被覆盖。`;
  if (!confirm(msg)) return;

  const label = btn.textContent;
  btn.disabled = true;
  btn.textContent = isUp ? '上传中…' : '下载中…';
  try {
    const r = await invoke('cloud_sync_one', { relPath: rel, direction: way });
    // 重新对照一遍,让状态徽章更新
    await loadCloudDiff();
    // 提示放在重绘之后 —— 否则被 innerHTML 冲掉
    logTo($('syncLog'), '↕ ' + r, 'ln-ok');
  } catch (err) {
    btn.disabled = false;
    btn.textContent = label;
    alert((isUp ? '上传失败:' : '下载失败:') + err);
  }
}

/** 按当前模式重新拉取云端数据。 */
async function reloadCloud() {
  if (CLOUD.mode === 'diff') {
    await loadCloudDiff();
  } else if (CLOUD.mode === 'tree') {
    await loadCloudTree();
  }
}

async function loadCloudTree() {
  const el = $('cloudTree');
  el.textContent = '读取云端…';
  try {
    const nodes = await invoke('cloud_tree', { relDir: null, depth: 3 });
    CLOUD.mode = 'tree';
    CLOUD.lastTree = nodes;
    if (!nodes.length) {
      el.textContent = '云端还没有任何文件。先跑一次同步。';
      return;
    }
    let files = 0;
    let bytes = 0;
    const count = (ns) => {
      for (const n of ns) {
        if (n.is_dir) count(n.children || []);
        else {
          files++;
          bytes += n.size || 0;
        }
      }
    };
    count(nodes);
    el.innerHTML =
      `<div class="cloud-row" style="opacity:.75"><span class="cloud-name">共 ${files} 个文件,${fmtBytes(bytes)}</span></div>` +
      cloudTreeHtml(nodes, 0);
  } catch (e) {
    el.textContent = '读取失败:' + e;
  }
}

async function loadCloudDiff() {
  const el = $('cloudTree');
  el.textContent = '对照中(需要读云端目录,可能要几秒)…';
  try {
    const nodes = await invoke('cloud_diff', { depth: 3 });
    CLOUD.mode = 'diff';
    CLOUD.lastTree = nodes;
    if (!nodes.length) {
      el.textContent = '两边都是空的。';
      return;
    }
    el.innerHTML =
      '<div class="cloud-row" style="opacity:.75"><span class="cloud-name">' +
      '状态:<span class="cloud-badge same">一致</span> ' +
      '<span class="cloud-badge differ">大小不同</span> ' +
      '<span class="cloud-badge local">仅本地</span> ' +
      '<span class="cloud-badge remote">仅云端</span>' +
      '</span></div>' +
      cloudDiffHtml(nodes, 0);
  } catch (e) {
    el.textContent = '对照失败:' + e;
  }
}

$('btnCloudTree').addEventListener('click', loadCloudTree);
$('btnCloudDiff').addEventListener('click', loadCloudDiff);

async function loadSettings() {
  try {
    const c = await invoke('get_llm_config');
    $('llmBaseUrl').value = c.base_url;
    $('llmModel').value = c.model;
    $('llmKeyHint').textContent = c.key_from_env
      ? '(来自环境变量 RECSUM_API_KEY)'
      : c.key_masked
      ? `(已保存:${c.key_masked})`
      : '(未配置)';
    $('llmKey').placeholder = c.key_masked ? '留空则不修改' : 'sk-…';

    const preset = $('llmPreset');
    preset.innerHTML = '<option value="">自定义</option>';
    c.presets.forEach((p) => {
      const o = document.createElement('option');
      o.value = p.url;
      o.textContent = p.name;
      o.dataset.models = JSON.stringify(p.models);
      preset.appendChild(o);
      if (p.url === c.base_url) o.selected = true;
    });

    applyPresetModels();

    const st = await invoke('get_stats');
    $('statsBox').innerHTML = `
      <div class="hw-row"><span>会话数</span><b>${st.sessions}</b></div>
      <div class="hw-row"><span>转写缓存块</span><b>${st.transcript_chunks}</b></div>
      <div class="hw-row"><span>说话人区分缓存</span><b>${st.diarize_results}</b></div>
      <div class="hw-row"><span>登记样本</span><b>${st.enrollment_samples}</b></div>
      <div class="hw-row"><span>待同步文本</span><b>${st.sync_files} 个 · ${fmtBytes(
      st.sync_bytes
    )}</b></div>
    `;

    // 前端依赖检查:mermaid 缺失时导图渲染不了,必须让用户看见
    try {
      const deps = await invoke('check_frontend_deps');
      const box = $('depsBox');
      if (box) {
        if (deps.mermaid_present) {
          box.innerHTML = `<div class="hw-row"><span>✅ mermaid(思维导图)</span><b>${deps.mermaid_size_kb} KB</b></div>`;
        } else {
          box.innerHTML =
            `<div class="hw-warn">⚠ 未找到 mermaid.min.js,思维导图无法渲染(会降级为文本大纲)。<br><br>` +
            `下载方式见项目里的 <code>ui/vendor/README.txt</code>。</div>`;
        }
      }
    } catch (e) {
      // 检查失败不阻塞设置页
      console.warn('前端依赖检查失败:', e);
    }
  } catch (e) {
    logTo($('llmLog'), '加载失败:' + e, 'ln-warn');
  }
}

function applyPresetModels() {
  const p = $('llmPreset');
  const opt = p.options[p.selectedIndex];
  const dl = $('llmModelList');
  dl.innerHTML = '';
  let models = [];
  try {
    models = opt && opt.dataset.models ? JSON.parse(opt.dataset.models) : [];
  } catch (_) {}
  models.forEach((m) => {
    const o = document.createElement('option');
    o.value = m;
    dl.appendChild(o);
  });
}

$('llmPreset').addEventListener('change', () => {
  const p = $('llmPreset');
  if (p.value) {
    $('llmBaseUrl').value = p.value;
    applyPresetModels();
    const first = $('llmModelList').firstChild;
    if (first) $('llmModel').value = first.value;
  }
});

$('btnLlmSave').addEventListener('click', async () => {
  show($('llmLog'));
  try {
    await invoke('set_llm_config', {
      baseUrl: $('llmBaseUrl').value,
      model: $('llmModel').value,
      provider: $('llmPreset').selectedIndex > 0 ? $('llmPreset').value : 'custom',
      apiKey: $('llmKey').value || null,
    });
    $('llmKey').value = '';
    logTo($('llmLog'), '✓ 已保存', 'ln-ok');
    loadSettings();
  } catch (e) {
    logTo($('llmLog'), '✗ ' + e, 'ln-warn');
  }
});

$('btnLlmTest').addEventListener('click', async () => {
  show($('llmLog'));
  logTo($('llmLog'), '正在测试…');
  try {
    const r = await invoke('test_llm');
    logTo($('llmLog'), '✓ 连接正常,模型回复:' + r, 'ln-ok');
  } catch (e) {
    logTo($('llmLog'), '✗ ' + e, 'ln-warn');
  }
});

$('btnLlmClear').addEventListener('click', async () => {
  show($('llmLog'));
  try {
    await invoke('clear_api_key');
    logTo($('llmLog'), '✓ 已清除 API Key', 'ln-ok');
    loadSettings();
  } catch (e) {
    logTo($('llmLog'), '✗ ' + e, 'ln-warn');
  }
});

$('btnRefreshHw').addEventListener('click', loadHardware);

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

/** 把致命错误显示在界面上,而不是只留在控制台里。
 *  之前遇到过"界面正常但数据不加载"的情况 —— 那是 __TAURI__ 未注入导致的
 *  脚本早期异常,而界面上完全看不出来。这个函数就是为了避免那种沉默失败。 */
function fatal(msg) {
  const box = document.getElementById('hwCard');
  if (box) {
    box.innerHTML = `<div class="hw-warn">⚠ 前端初始化失败<br><br>${escapeHtml(
      msg
    )}<br><br>如果反复出现,请检查 Tauri 配置中的 withGlobalTauri 是否开启。</div>`;
  }
  console.error('[录音转总结] fatal:', msg);
}

(async function init() {
  // 先确认 Tauri 注入的 API 存在 —— 否则后面全是静默失败
  if (!window.__TAURI__) {
    fatal('未检测到 window.__TAURI__。Tauri 的全局 API 未注入。');
    return;
  }
  if (!window.__TAURI__.core || !window.__TAURI__.core.invoke) {
    fatal('window.__TAURI__.core.invoke 不存在(API 版本不匹配)。');
    return;
  }

  try {
    await loadHardware();
  } catch (e) {
    fatal(String(e && e.stack ? e.stack : e));
  }

  // `--diag`:显示运行时诊断浮层。
  //
  // 由 Rust 侧在带 --diag 启动时注入 window.__DIAG__ 打开。
  // 没有 devtools 时这是唯一能看到前端内部状态的办法 ——
  // 排查"图标不显示""导图不渲染""拖放不生效"都靠它。
  if (window.__DIAG__) {
    const box = document.createElement('div');
    box.id = 'diagBox';
    box.style.cssText =
      'position:fixed;right:0;bottom:0;z-index:99999;background:#000d;color:#0f0;' +
      'font:11px/1.5 monospace;padding:6px 8px;white-space:pre;pointer-events:none;' +
      'max-width:60vw;border-top-left-radius:6px';
    document.body.appendChild(box);

    setInterval(() => {
      const d = window.__DRAG_DROP__ || {};
      const cs = getComputedStyle(document.documentElement);
      const rootPx = parseFloat(cs.fontSize);
      const sb = document.querySelector('.sidebar');
      const sbW = sb ? sb.getBoundingClientRect().width : 0;
      // 侧栏宽度 = 14.75rem,所以 sbW/14.75 就是实际生效的根字号。
      // 与 getComputedStyle 报的值对上,才说明 rem 基准一致。
      const impliedRoot = sbW > 0 ? (sbW / 14.75).toFixed(2) : '?';
      const va = parseFloat(getComputedStyle(document.querySelector('.brand-title')).fontSize);
      box.textContent = [
        'DIAG',
        `window=${window.innerWidth}x${window.innerHeight}  dpr=${window.devicePixelRatio}`,
        `rootFontSize=${rootPx}px   侧栏反推=${impliedRoot}px`,
        `sidebar=${Math.round(sbW)}px   .brand-title=${va}px`,
        `html.style.fontSize='${document.documentElement.style.fontSize || "(空)"}'`,
        `matchMedia(1000px)=${window.matchMedia('(min-width: 1000px)').matches}`,
        `dragdrop=${d.mode} ready=${d.ready}`,
        `pickedPath=${S.pickedPath || '(未选)'}`,
        `mermaid=${window.mermaid ? 'yes' : 'NO'}`,
      ].join('\n');
    }, 500);
  }
})();
