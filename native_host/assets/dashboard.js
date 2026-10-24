(() => {
  'use strict';
  const byId = (id) => document.getElementById(id);
  const state = { data: null, sort: ['persistent_bytes', -1], timer: null };

  function humanBytes(value) {
    if (value === null || value === undefined) return '—';
    const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
    let n = Number(value), unit = 0;
    while (n >= 1024 && unit < units.length - 1) {
      n /= 1024;
      unit += 1;
    }
    return unit === 0 ? n.toLocaleString() + ' B' : n.toFixed(2) + ' ' + units[unit];
  }

  function num(value) {
    return Number(value || 0).toLocaleString();
  }

  function elapsed(ms) {
    if (ms < 1000) return Math.round(ms) + ' ms';
    if (ms < 60000) return (ms / 1000).toFixed(1) + ' s';
    return (ms / 60000).toFixed(1) + ' min';
  }

  function authPayload() {
    try {
      return JSON.parse(sessionStorage.getItem('falkordb.dashboardAuth') || 'null');
    } catch (_) {
      return null;
    }
  }

  function saveAuthPayload(payload) {
    sessionStorage.setItem('falkordb.dashboardAuth', JSON.stringify(payload));
  }

  function clearAuthPayload() {
    sessionStorage.removeItem('falkordb.dashboardAuth');
  }

  async function dashboardOverview(payload = authPayload()) {
    if (!payload) throw new Error('Enter dashboard credentials.');
    const response = await fetch('/dashboard/overview', {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'Accept': 'application/json'
      },
      body: JSON.stringify(payload),
      cache: 'no-store'
    });
    const body = await response.json().catch(() => ({}));
    if (!response.ok) {
      throw new Error(body && body.error && body.error.message
        ? body.error.message
        : 'HTTP ' + response.status);
    }
    return body;
  }

  function card(label, value, sub) {
    const el = document.createElement('div');
    el.className = 'card';
    const l = document.createElement('div');
    l.className = 'label';
    l.textContent = label;
    const v = document.createElement('div');
    v.className = 'value';
    v.textContent = value;
    el.append(l, v);
    if (sub) {
      const s = document.createElement('div');
      s.className = 'sub';
      s.textContent = sub;
      el.append(s);
    }
    return el;
  }

  function detail(name, value) {
    const el = document.createElement('div');
    el.className = 'detail';
    const n = document.createElement('div');
    n.className = 'name';
    n.textContent = name;
    const v = document.createElement('div');
    v.className = 'num';
    v.textContent = value;
    el.append(n, v);
    return el;
  }

  function renderSummary(data) {
    const graphs = data.database.graphs || [];
    const totals = graphs.reduce((acc, graph) => {
      acc.nodes += Number(graph.nodes || 0);
      acc.relationships += Number(graph.relationships || 0);
      acc.memory += Number(graph.estimated_memory_bytes || 0);
      return acc;
    }, { nodes: 0, relationships: 0, memory: 0 });

    byId('summaryCards').replaceChildren(
      card('Graphs', num(graphs.length), 'API ' + data.server.api_version),
      card('Nodes', num(totals.nodes), ''),
      card('Relationships', num(totals.relationships), ''),
      card('Database', humanBytes(data.database.total_storage_bytes), 'persistent data'),
      card('Memory est.', humanBytes(totals.memory), num(data.database.memory_samples) + ' samples/graph'),
      card('Queries', data.queries.running.length + ' / ' + data.queries.waiting.length, 'running / waiting')
    );
  }

  function renderGraphs(data) {
    const filter = byId('graphFilter').value.trim().toLowerCase();
    let graphs = Array.from(data.database.graphs || []);
    if (filter) {
      graphs = graphs.filter((graph) => graph.graph.toLowerCase().includes(filter));
    }

    const key = state.sort[0];
    const dir = state.sort[1];
    graphs.sort((a, b) => {
      const av = a[key] === null || a[key] === undefined ? -1 : a[key];
      const bv = b[key] === null || b[key] === undefined ? -1 : b[key];
      if (typeof av === 'string') return av.localeCompare(bv) * dir;
      return (Number(av) - Number(bv)) * dir;
    });

    const body = byId('graphsBody');
    body.replaceChildren();
    for (const graph of graphs) {
      const row = document.createElement('tr');
      const values = [
        graph.graph,
        num(graph.nodes),
        num(graph.relationships),
        num(graph.graph_version),
        num(graph.schema_version),
        num(graph.labels),
        num(graph.relationship_types),
        num(graph.property_keys),
        num(graph.index_definitions),
        num(graph.constraints),
        humanBytes(graph.persistent_bytes),
        humanBytes(graph.wal_bytes),
        humanBytes(graph.checkpoint_bytes) + ' (' + num(graph.checkpoint_count) + ')',
        humanBytes(graph.estimated_memory_bytes)
      ];
      values.forEach((value, index) => {
        const cell = document.createElement('td');
        cell.textContent = value;
        if (index === 0) cell.className = 'graph-name';
        row.append(cell);
      });
      body.append(row);
    }
  }

  function renderQueryList(target, items) {
    const root = byId(target);
    root.replaceChildren();
    if (!items.length) {
      root.className = 'query-list empty';
      root.textContent = 'None';
      return;
    }
    root.className = 'query-list';
    for (const query of items) {
      const item = document.createElement('div');
      item.className = 'query-item';
      const meta = document.createElement('div');
      meta.className = 'query-meta';
      const left = document.createElement('span');
      left.textContent = '#' + query.id + ' · ' + query.graph;
      const right = document.createElement('span');
      right.textContent = elapsed(query.elapsed_ms);
      meta.append(left, right);
      const code = document.createElement('div');
      code.className = 'query-code';
      code.textContent = query.query;
      item.append(meta, code);
      root.append(item);
    }
  }

  function renderQueries(data) {
    byId('runningCount').textContent = num(data.queries.running.length);
    byId('waitingCount').textContent = num(data.queries.waiting.length);
    renderQueryList('runningQueries', data.queries.running);
    renderQueryList('waitingQueries', data.queries.waiting);
  }

  function renderSlowQueries(data) {
    const items = data.queries.slow || [];
    const root = byId('slowQueries');
    byId('slowCount').textContent = num(items.length);
    root.replaceChildren();
    if (!items.length) {
      root.className = 'query-list empty';
      root.textContent = 'No slow queries recorded';
      return;
    }
    root.className = 'query-list';
    for (const query of items) {
      const item = document.createElement('div');
      item.className = 'query-item';
      const meta = document.createElement('div');
      meta.className = 'query-meta';
      const left = document.createElement('span');
      left.textContent = query.graph + ' · ' + query.command;
      const right = document.createElement('span');
      right.textContent = Number(query.latency_ms).toFixed(2) + ' ms · ' +
        new Date(Number(query.timestamp) * 1000).toLocaleTimeString();
      meta.append(left, right);
      const code = document.createElement('div');
      code.className = 'query-code';
      code.textContent = query.query;
      item.append(meta, code);
      if (query.params) {
        const params = document.createElement('div');
        params.className = 'query-params';
        params.textContent = query.params;
        item.append(params);
      }
      root.append(item);
    }
  }

  function renderStorage(data) {
    const db = data.database;
    byId('storageDetails').replaceChildren(
      detail('Total data dir', humanBytes(db.total_storage_bytes)),
      detail('Graphs dir', humanBytes(db.graphs_storage_bytes)),
      detail('Attributed graphs', humanBytes(db.attributed_graph_bytes)),
      detail('Unattributed residue', humanBytes(db.unattributed_graph_storage_bytes)),
      detail('Import staging', humanBytes(db.imports_storage_bytes)),
      detail('Other data', humanBytes(db.other_storage_bytes)),
      detail('CPU parallelism', num(data.server.available_parallelism)),
      detail('Host version', data.server.host_version),
      detail('MCP toolset', data.server.mcp_toolset_version || '—')
    );

    const warning = byId('storageWarning');
    const orphan = Number(db.unattributed_graph_storage_bytes || 0);
    if (orphan > 0) {
      warning.hidden = false;
      warning.textContent = 'Storage warning: ' + humanBytes(orphan) +
        ' in the graph directory is not attributed to currently loaded WAL/checkpoint files.';
    } else {
      warning.hidden = true;
      warning.textContent = '';
    }
  }

  function render(data) {
    state.data = data;
    renderSummary(data);
    renderGraphs(data);
    renderQueries(data);
    renderSlowQueries(data);
    renderStorage(data);
    byId('lastUpdated').textContent = ' · ' + new Date(data.generated_at_ms).toLocaleTimeString();
  }

  async function refresh() {
    try {
      const data = await dashboardOverview();
      render(data);
      byId('connectionDot').style.background = 'var(--good)';
      byId('connectionText').textContent = 'Connected';
    } catch (error) {
      byId('connectionDot').style.background = 'var(--danger)';
      byId('connectionText').textContent = 'Error: ' + error.message;
    }
  }

  function resetTimer() {
    if (state.timer) clearInterval(state.timer);
    state.timer = null;
    const ms = Number(byId('refreshInterval').value);
    if (ms > 0) state.timer = setInterval(refresh, ms);
  }

  byId('connectButton').addEventListener('click', async () => {
    const apiToken = byId('tokenInput').value.trim();
    const username = byId('viewerUsername').value.trim();
    const password = byId('viewerPassword').value;

    let payload;
    if (apiToken) {
      payload = {
        viewer_username: '',
        viewer_password: '',
        api_token: apiToken
      };
    } else {
      if (!username || !password) {
        byId('loginError').textContent =
          'Enter the viewer username/password or a read-only API token.';
        byId('loginError').hidden = false;
        return;
      }
      payload = {
        viewer_username: username,
        viewer_password: password,
        api_token: ''
      };
    }

    saveAuthPayload(payload);
    byId('loginError').hidden = true;
    try {
      const data = await dashboardOverview(payload);
      byId('loginPanel').hidden = true;
      byId('dashboard').hidden = false;
      byId('tokenInput').value = '';
      byId('viewerPassword').value = '';
      render(data);
      resetTimer();
    } catch (error) {
      clearAuthPayload();
      byId('loginError').textContent = error.message;
      byId('loginError').hidden = false;
    }
  });

  ['tokenInput', 'viewerPassword'].forEach((id) => {
    byId(id).addEventListener('keydown', (event) => {
      if (event.key === 'Enter') byId('connectButton').click();
    });
  });
  byId('disconnectButton').addEventListener('click', () => {
    clearAuthPayload();
    if (state.timer) clearInterval(state.timer);
    byId('dashboard').hidden = true;
    byId('loginPanel').hidden = false;
  });
  byId('refreshButton').addEventListener('click', refresh);
  byId('refreshInterval').addEventListener('change', resetTimer);
  byId('graphFilter').addEventListener('input', () => {
    if (state.data) renderGraphs(state.data);
  });
  document.querySelectorAll('th[data-sort]').forEach((header) => {
    header.addEventListener('click', () => {
      const key = header.dataset.sort;
      state.sort = state.sort[0] === key
        ? [key, state.sort[1] * -1]
        : [key, key === 'graph' ? 1 : -1];
      if (state.data) renderGraphs(state.data);
    });
  });

  if (authPayload()) {
    dashboardOverview().then((data) => {
      byId('loginPanel').hidden = true;
      byId('dashboard').hidden = false;
      render(data);
      resetTimer();
    }).catch(() => clearAuthPayload());
  }
})();