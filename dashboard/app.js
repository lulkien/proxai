const API = '/dashboard/api';

function fmtNum(n) { return n.toLocaleString('en'); }

async function fetchJSON(url, opts) {
  const res = await fetch(url, opts);
  const data = await res.json();
  if (!res.ok) throw new Error(data.error || res.statusText);
  return data;
}

// ── Tabs ──

function switchTab(tab) {
  document.querySelectorAll('.tab-btn').forEach((b, i) =>
    b.classList.toggle('active', (i === 0 ? 'usage' : 'keys') === tab));
  if (tab === 'usage') loadUsage(); else loadKeys();
}

// ── Usage ──

async function loadUsage() {
  const el = document.getElementById('content');
  el.innerHTML = '<div id="loading">Loading...</div>';
  try {
    const data = await fetchJSON(API + '/stats');
    const total = data.keys.reduce((s, k) => s + k.total_requests, 0);
    el.innerHTML = renderUsage(data, total);
    if (data.keys.length > 0) loadTimeline('1d');
  } catch (e) {
    el.innerHTML = '<div class="error-msg">' + e.message + '</div>';
  }
}

function renderUsage(data, total) {
  let rows = data.keys.map(k => {
    let badges = Object.entries(k.model_usage).map(([m, u]) =>
      '<span class="model-badge">' + m + ': ' + fmtNum(u.requests) + ' req</span>'
    ).join('');
    return '<tr class="usage-row" onclick="let d=this.nextElementSibling;if(d)d.classList.toggle(\'show\');let i=this.querySelector(\'.toggle-icon\');if(i)i.classList.toggle(\'open\')">'
      + '<td class="key-name">' + (data.keys.length > 1 ? '<span class="toggle-icon">\u25b6</span> ' : '') + k.key_name + '</td>'
      + '<td>' + fmtNum(k.total_requests) + '</td>'
      + '<td>' + (k.last_used || 'never') + '</td>'
      + '</tr>'
      + (badges ? '<tr class="model-detail"><td colspan="3">' + badges + '</td></tr>' : '');
  }).join('');

  return '<div class="cards">'
    + '<div class="card"><div class="label">Total Requests</div><div class="value">' + fmtNum(total) + '</div></div>'
    + '<div class="card"><div class="label">Active Keys</div><div class="value">' + data.keys.length + '</div></div>'
    + '</div>'
    + (data.keys.length === 0
      ? '<div class="empty">No usage yet. Send a request to start tracking.</div>'
      : '<div class="chart-section">'
        + '<div class="chart-range">'
          + '<button class="range-btn active" onclick="loadTimeline(\'1d\')">1D</button>'
          + '<button class="range-btn" onclick="loadTimeline(\'7d\')">7D</button>'
        + '</div>'
        + '<div class="chart-wrap" id="chart-wrap"><div class="chart-empty">Loading chart...</div></div>'
      + '</div>'
      + '<table><thead><tr><th>Key</th><th>Requests</th><th>Last Used</th></tr></thead><tbody>' + rows
        + '</tbody></table><div class="updated">Updated: ' + data.updated_at + '</div>');
}

// ── Chart ──

const CHART_COLORS = ['#58a6ff', '#3fb950', '#f0883e', '#d2a8ff', '#f85149', '#ffa657'];
let colorMap = {};

function getColor(name) {
  if (!colorMap[name]) colorMap[name] = CHART_COLORS[Object.keys(colorMap).length % CHART_COLORS.length];
  return colorMap[name];
}

async function loadTimeline(range) {
  document.querySelectorAll('.range-btn').forEach(b => {
    var a = b.getAttribute('onclick') || '';
    b.classList.toggle('active', a.indexOf("'" + range + "'") !== -1);
  });
  try {
    var buckets = await fetchJSON(API + '/stats/timeline?range=' + range);
    renderChart(buckets, range);
  } catch (e) {
    document.getElementById('chart-wrap').innerHTML = '<div class="chart-empty">Chart unavailable</div>';
  }
}

function renderChart(buckets, range) {
  var wrap = document.getElementById('chart-wrap');
  if (!buckets || !buckets.length) {
    wrap.innerHTML = '<div class="chart-empty">No data for this period</div>';
    return;
  }

  // Build color map from all keys across buckets (stable assignment)
  colorMap = {};
  for (var i = 0; i < buckets.length; i++) {
    for (var j = 0; j < buckets[i].keys.length; j++) {
      getColor(buckets[i].keys[j].key_name);
    }
  }

  // Max total requests per bucket for height normalization
  var maxTotal = 0;
  for (var i = 0; i < buckets.length; i++) {
    var t = 0;
    for (var j = 0; j < buckets[i].keys.length; j++) t += buckets[i].keys[j].requests;
    if (t > maxTotal) maxTotal = t;
  }
  if (maxTotal === 0) maxTotal = 1;

  var bars = '';
  for (var i = 0; i < buckets.length; i++) {
    var b = buckets[i];
    var total = 0;
    for (var j = 0; j < b.keys.length; j++) total += b.keys[j].requests;
    var heightPct = (total / maxTotal) * 100;

    var segments = '';
    for (var j = 0; j < b.keys.length; j++) {
      var k = b.keys[j];
      var pct = total > 0 ? (k.requests / total) * 100 : 0;
      segments += '<div class="chart-segment" style="height:' + pct.toFixed(1) + '%;background:' + getColor(k.key_name)
        + '" data-tip="' + k.key_name + ': ' + fmtNum(k.requests) + ' req"></div>';
    }

    var label = range === '1d' ? b.time.slice(11) : b.time.slice(5);
    bars += '<div class="chart-bar">'
      + '<div class="chart-bar-fill" style="height:' + heightPct.toFixed(1) + '%">' + segments + '</div>'
      + '<div class="chart-label">' + label + '</div>'
      + '</div>';
  }

  var legend = '';
  var names = Object.keys(colorMap);
  for (var i = 0; i < names.length; i++) {
    legend += '<span class="legend-item"><span class="legend-dot" style="background:' + colorMap[names[i]] + '"></span>' + names[i] + '</span>';
  }

  wrap.innerHTML = '<div class="chart-bars">' + bars + '</div>'
    + '<div class="chart-legend">' + legend + '</div>'
    + '<div class="chart-tooltip" id="chart-tooltip" style="display:none"></div>';

  // Tooltip
  var tooltip = document.getElementById('chart-tooltip');
  var segs = wrap.querySelectorAll('.chart-segment');
  for (var i = 0; i < segs.length; i++) {
    segs[i].addEventListener('mouseenter', function(e) {
      tooltip.textContent = this.getAttribute('data-tip');
      tooltip.style.display = 'block';
    });
    segs[i].addEventListener('mousemove', function(e) {
      var r = wrap.getBoundingClientRect();
      tooltip.style.left = (e.clientX - r.left + 12) + 'px';
      tooltip.style.top = (e.clientY - r.top - 20) + 'px';
    });
    segs[i].addEventListener('mouseleave', function() {
      tooltip.style.display = 'none';
    });
  }
}

// ── Keys ──

function renderKeysTab() {
  return '<h2>Generate New Key</h2>'
    + '<div class="form-row">'
    + '<input id="key-name-input" placeholder="Key name (e.g. hermes)" onkeydown="if(event.key===\'Enter\')generateKey()">'
    + '<button class="btn" onclick="generateKey()">Generate</button>'
    + '</div>'
    + '<div id="gen-error" class="error-msg" style="display:none"></div>'
    + '<div id="gen-result" style="display:none"></div>'
    + '<h2 class="section-head">Existing Keys</h2>'
    + '<div id="keys-table"><div id="loading">Loading...</div></div>';
}

async function loadKeys() {
  const el = document.getElementById('content');
  el.innerHTML = renderKeysTab();
  try {
    const keys = await fetchJSON(API + '/keys');
    renderKeyList(keys);
  } catch (e) {
    document.getElementById('keys-table').innerHTML = '<div class="error-msg">' + e.message + '</div>';
  }
}

function renderKeyList(keys) {
  const el = document.getElementById('keys-table');
  if (keys.length === 0) {
    el.innerHTML = '<div class="empty">No keys yet.</div>';
    return;
  }
  el.innerHTML = '<table><thead><tr><th>ID</th><th>Name</th><th>Partial</th><th>Created</th><th></th></tr></thead><tbody>'
    + keys.map(k => '<tr>'
      + '<td>' + k.id + '</td>'
      + '<td class="key-name">' + k.name + '</td>'
      + '<td>' + k.partial + '</td>'
      + '<td>' + k.created_at + '</td>'
      + '<td><button class="btn-danger" onclick="revokeKey(\'' + k.name + '\')">Revoke</button></td>'
      + '</tr>').join('')
    + '</tbody></table>';
}

async function generateKey() {
  const name = document.getElementById('key-name-input').value.trim();
  if (!name) return;
  const errEl = document.getElementById('gen-error');
  errEl.style.display = 'none';
  try {
    const data = await fetchJSON(API + '/keys/generate', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name: name })
    });
    document.getElementById('gen-result').style.display = 'block';
    document.getElementById('gen-result').innerHTML =
      '<p style="color:#3fb950;font-weight:600;margin-bottom:.75rem">API key generated (shown once!)</p>'
      + '<input style="width:100%;background:#0d1117;border:1px solid #58a6ff;padding:.5rem .75rem;border-radius:6px;font-family:monospace;font-size:.8rem;margin-bottom:1rem" value="' + data.key + '" readonly onclick="this.select()">'
      + '<div style="display:flex;gap:.5rem;justify-content:flex-end">'
      + '<button class="btn-sm" onclick="document.getElementById(\'gen-result\').style.display=\'none\'">Close</button>'
      + '<button class="btn btn-sm" style="background:#21262d;border:1px solid #30363d" onclick="navigator.clipboard.writeText(\'' + data.key + '\')">Copy</button>'
      + '</div>';
    document.getElementById('key-name-input').value = '';
    loadKeyList();
  } catch (e) {
    errEl.textContent = e.message;
    errEl.style.display = 'block';
  }
}

async function revokeKey(target) {
  if (!confirm('Revoke key "' + target + '"? This cannot be undone.')) return;
  try {
    await fetchJSON(API + '/keys/revoke', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ target: target })
    });
    loadKeyList();
  } catch (e) {
    document.getElementById('keys-table').innerHTML = '<div class="error-msg">' + e.message + '</div>';
  }
}

async function loadKeyList() {
  try {
    const keys = await fetchJSON(API + '/keys');
    renderKeyList(keys);
  } catch (e) {
    document.getElementById('keys-table').innerHTML = '<div class="error-msg">' + e.message + '</div>';
  }
}

// ── Init ──
loadUsage();
