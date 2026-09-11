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
['dragenter', 'dragover'].forEach((ev) =>
  dz.addEventListener(ev, (e) => { e.preventDefault(); dz.classList.add('over'); })
);
['dragleave', 'drop'].forEach((ev) =>
  dz.addEventListener(ev, (e) => { e.preventDefault(); dz.classList.remove('over'); })
);
dz.addEventListener('drop', (e) => {
  const f = e.dataTransfer.files[0];
  if (f) setPicked(f.path || f.name);
});

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

listen('pipeline://progress', (ev) => {
  const p = ev.payload;
  if (p.kind === 'stage_start') {
    $('progStage').textContent = p.stage + '…';
  } else if (p.kind === 'stage_pct') {
    if (p.pct >= 1) {
      $('progPct').textContent = '';
      logTo($('progLog'), '✓ ' + p.stage, 'ln-ok');
    } else {
      $('progBar').style.width = Math.round(p.pct * 100) + '%';
    }
  } else if (p.kind === 'transcribe') {
    const pct = p.total_ms ? p.done_ms / p.total_ms : 0;
    $('progBar').style.width = Math.round(pct * 100) + '%';
    $('progPct').textContent = Math.round(pct * 100) + '%';
    $('progDetail').textContent =
      '已处理 ' + fmtClock(p.done_ms) + ' / ' + fmtClock(p.total_ms);
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
    const op = window.__TAURI__ && window.__TAURI__.opener;
    if (op && op.openPath) {
      await op.openPath(dir);
    } else {
      alert('工程目录:\n' + dir);
    }
  } catch (e) {
    alert('打开目录失败:' + e);
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
// 设置
// ---------------------------------------------------------------------------

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
})();
