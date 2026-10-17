(() => {
  'use strict';
  const $ = id => document.getElementById(id);
  const node = (tag, text, cls) => {
    const result = document.createElement(tag);
    if (text !== undefined) result.textContent = text;
    if (cls) result.className = cls;
    return result;
  };
  const pretty = value => typeof value === 'string' ? value : JSON.stringify(value, null, 2);
  const short = (value, limit = 220) => {
    const text = pretty(value) || '';
    return text.length > limit ? text.slice(0, limit) + '…' : text;
  };
  const number = value => Number(value || 0).toLocaleString();
  const duration = value => `${Math.floor(Math.max(0, value) / 60)}m ${Math.floor(Math.max(0, value) % 60)}s`;
  const details = (label, text) => {
    const element = node('details');
    element.append(node('summary', label), node('pre', pretty(text)));
    return element;
  };
  let token = document.querySelector('meta[name=playtime-token]').content;
  const runId = document.body.dataset.run;
  async function api(path, body, retry = true) {
    const options = body === undefined ? {} : {
      method: 'POST', headers: {'Content-Type': 'application/json', 'X-Playtime-Token': token},
      body: JSON.stringify(body)
    };
    const response = await fetch(path, {...options, signal: AbortSignal.timeout(30000)});
    const data = await response.json();
    if (response.status === 403 && body !== undefined && retry) {
      token = (await api('/api/bootstrap')).token;
      return api(path, body, false);
    }
    if (!response.ok) throw new Error(data.error || `HTTP ${response.status}`);
    return data;
  }
  let transientError = false;
  function error(message, transient = false) {
    transientError = transient; $('error').textContent = message; $('error').hidden = !message;
  }
  function connected() {
    $('connection').textContent = 'Connected locally';
    if (transientError) error('');
  }
  let preview, busy = false, launching = false, previewVersion = 0;
  function launchState() { $('launch').disabled = busy || launching || !preview; }
  async function loadPreview() {
    const version = ++previewVersion;
    preview = null; launchState();
    try {
      const data = await api(`/api/preview?recipe=${encodeURIComponent($('recipe').value)}&mode=${$('mode').value}`);
      if (version !== previewVersion) return;
      preview = data;
      const r = preview.recipe;
      $('recipe-meta').textContent = `${r.model.name} · ${r.model.think} thinking · ${number(r.model.context_tokens)} context · up to ${r.sessions} sessions · ${number(r.limits.generated_tokens)} generated tokens · ${duration(r.limits.wall_seconds)}`;
      $('prompt').textContent = preview.prompt;
      $('protocol').textContent = preview.protocol;
      $('settings').textContent = pretty(r);
      connected(); error('');
    } catch (err) { error(err.message); }
    launchState();
  }
  async function refreshRuns() {
    try {
      const data = await api('/api/runs');
      busy = data.busy;
      $('launch-note').textContent = busy ? 'An experiment is active. Open it below to follow along.' : 'Inputs are frozen when you launch.';
      launchState();
      const cards = data.runs.map(run => {
        const card = node('a', undefined, 'run-card' + (run.active ? ' active' : ''));
        card.href = '/watch/' + run.id;
        card.append(node('span', run.active ? run.state : run.result?.stop_reason || run.state, 'badge' + (run.active ? ' good' : '')),
          node('h2', run.name), node('div', `${run.kind} · ${run.id}`, 'meta'));
        if (run.result) card.append(node('div', `${number(run.result.generated_tokens)} confirmed tokens${run.result.token_usage_complete === false ? ' + unknown usage' : ''} · ${run.result.sessions.length} sessions · ${duration(run.result.elapsed_seconds)}`, 'meta'));
        if (run.error) card.append(node('div', short(run.error), 'error'));
        return card;
      });
      $('runs').replaceChildren(...(cards.length ? cards : [node('p', 'No experiments yet.', 'muted')]));
      connected();
    } catch (err) { $('connection').textContent = 'Reconnecting…'; error(err.message, true); }
    setTimeout(refreshRuns, 4000);
  }
  $('recipe').addEventListener('change', loadPreview);
  $('mode').addEventListener('change', loadPreview);
  $('launch').addEventListener('click', async () => {
    if (!preview || busy || launching) return;
    launching = true; launchState(); $('launch').textContent = 'Freezing inputs…';
    try {
      const run = await api('/api/launch', {recipe: preview.path, mode: preview.mode, revision: preview.revision});
      location.assign(run.url);
    } catch (err) {
      launching = false; launchState(); $('launch').textContent = 'Launch experiment'; error(err.message);
    }
  });

  let cursor = 0, current, status, phase = 'Starting', session = 0, generated = 0, charged = 0, pending = 0;
  let started, lastChunk, following = true, lastHealth = 0;
  const turns = new Map(), checkpoints = new Map(), dirty = new Set();
  function followState() {
    $('follow').textContent = following ? 'Following live' : 'Follow live';
    $('follow').setAttribute('aria-pressed', String(following));
  }
  function scrollToLive() { if (following && current) current.card.scrollIntoView({block: 'end'}); }
  $('follow').addEventListener('click', () => { following = !following; followState(); scrollToLive(); });
  const pauseFollow = () => { following = false; followState(); };
  // React to the reader's input, not scroll anchoring when completed turns shrink.
  window.addEventListener('wheel', event => { if (event.deltaY < 0) pauseFollow(); }, {passive: true});
  window.addEventListener('keydown', event => {
    if (['ArrowUp', 'PageUp', 'Home'].includes(event.key)) pauseFollow();
  });
  let touchY;
  window.addEventListener('touchstart', event => { touchY = event.touches[0]?.clientY; }, {passive: true});
  window.addEventListener('touchmove', event => {
    if (event.touches[0]?.clientY > touchY + 5) pauseFollow();
  }, {passive: true});
  window.addEventListener('pointerdown', event => {
    if (event.clientX >= document.documentElement.clientWidth) pauseFollow();
  });
  $('timeline').addEventListener('click', event => { if (event.target.closest('summary')) pauseFollow(); });
  function makeTurn(event) {
    if (current) { current.card.classList.remove('current'); current.details.open = false; }
    const card = node('article', undefined, 'turn current');
    const expanded = node('details'); expanded.open = true;
    const title = node('summary', `Session ${event.session} · Call ${event.call} · ${event.handoff ? 'Handoff' : 'Generating'}`);
    const brief = node('div', 'Waiting for the model…', 'turn-brief');
    const body = node('div', undefined, 'turn-body');
    const thinking = details('Thinking', ''); thinking.className = 'thinking';
    const content = node('pre'); content.hidden = true;
    body.append(thinking, content); expanded.append(title, body); card.append(expanded, brief);
    $('timeline').append(card);
    const turn = {card, details: expanded, title, brief, body, thinking, content, thought: '', text: '', tools: [], event};
    turns.set(event.call, turn); current = turn;
    return turn;
  }
  function record(event) {
    const kind = event.kind;
    if (kind === 'run_start') started = Date.parse(event.time);
    if (kind === 'session_start') {
      session = event.session;
      const marker = node('div', undefined, 'session-marker');
      marker.append(node('h3', `Session ${session} · Fresh conversation`),
        details('Memory received', event.memory?.content || 'No memento was available.'));
      $('timeline').append(marker);
    }
    if (kind === 'model_request') {
      makeTurn(event); pending = event.maximum;
      phase = event.handoff ? 'Saving handoff' : 'Generating';
    }
    const turn = turns.get(event.call) || current;
    if (kind === 'model_chunk') {
      lastChunk = Date.parse(event.time);
      if (turn) {
        turn.thought += event.message.thinking || '';
        turn.text += event.message.content || '';
        dirty.add(turn);
      }
    }
    if (kind === 'model_response') {
      generated = event.generated; charged = event.charged; pending = 0;
      if (turn) {
        turn.thought = event.message.thinking || turn.thought;
        turn.text = event.message.content || turn.text;
        turn.title.textContent = `Session ${event.session} · Call ${event.call} · ${number(event.eval_count)} tokens`;
        turn.brief.textContent = short(turn.text || 'Response received.'); dirty.add(turn);
      }
    }
    if (kind === 'tool_request' && turn) {
      phase = event.name === 'end_session' ? 'Saving memento' : 'Running tool';
      const element = node('div', undefined, 'live-tool');
      const label = node('div', event.name + ' · running', 'tool-label');
      const args = event.arguments;
      const action = args?.command || args?.path || (event.name === 'end_session' ? (args.finish_run ? 'Finish experiment' : 'Continue in a fresh session') : pretty(args));
      element.append(label, node('pre', short(action, 2000)), details('Tool arguments', args));
      turn.body.append(element); turn.brief.textContent = short(action);
      turn.tools.push({element, label, action, name: event.name});
    }
    if (kind === 'tool_result' && turn) {
      const tool = turn.tools[turn.tools.length - 1];
      if (tool) {
        const result = event.result;
        const failed = result.error || result.timed_out || (result.exit_code !== undefined && result.exit_code !== 0);
        tool.label.textContent = `${event.name} · ${failed ? 'failed' : 'complete'}`;
        if (failed) { tool.element.classList.add('failed'); turn.card.classList.add('failed'); }
        const output = result.error || [result.stdout, result.stderr].filter(Boolean).join('\n') || result.content || pretty(result);
        tool.element.append(node('pre', short(output, 16000)));
        turn.brief.textContent = short(tool.action, 130) + '\n' + short(output, 160);
      }
      phase = 'Between calls';
    }
    if (kind === 'model_error') {
      pending = 0; generated = event.generated_tokens || generated;
      charged = event.token_budget_charged || charged;
    }
    if (['model_error', 'handoff_missing', 'handoff_feedback', 'handoff_skipped'].includes(kind)) {
      (turn?.body || $('timeline')).append(node('p', event.error || event.content || kind.replaceAll('_', ' '), 'notice error'));
      if (turn) turn.card.classList.add('failed');
    }
    if (kind === 'checkpoint') {
      checkpoints.set(event.session, false); phase = 'Checkpoint saved';
      if (current) current.card.classList.remove('current');
    }
    if (kind === 'run_end') { generated = event.generated_tokens; charged = event.token_budget_charged; pending = 0; }
    $('waiting').hidden = true;
  }
  async function loadCheckpoint() {
    const next = [...checkpoints].find(([, loaded]) => !loaded);
    if (!next) return;
    const data = await api(`/api/run/${runId}/checkpoint/${next[0]}`);
    const card = node('section', undefined, 'checkpoint-card');
    card.dataset.session = String(data.session);
    card.append(node('h3', `Session ${data.session}`));
    const images = node('div', undefined, 'checkpoint-images');
    for (const artifact of data.images) {
      const figure = node('figure');
      if (artifact.image.src) {
        const button = node('button'); button.type = 'button'; button.setAttribute('aria-label', 'Enlarge ' + artifact.name);
        const image = node('img'); image.src = artifact.image.src; image.alt = artifact.name; image.loading = 'lazy';
        button.append(image); button.addEventListener('click', () => {
          pauseFollow();
          $('preview-name').textContent = artifact.name; $('preview-image').src = image.src; $('preview-image').alt = artifact.name;
          $('download-image').href = image.src; $('download-image').download = artifact.name.split('/').pop(); $('preview').showModal();
        }); figure.append(button);
      } else figure.append(node('p', artifact.image.notice, 'muted'));
      figure.append(node('figcaption', `${artifact.change} · ${artifact.name}`)); images.append(figure);
    }
    card.append(images, details('Memento at checkpoint', data.memento || 'No memento saved.'), details('File changes', data.summary.changes));
    if (data.summary.memento_status !== 'loaded') card.append(node('p', 'Memento unavailable: ' + data.summary.memento_status, 'error'));
    if (data.notice) card.append(node('p', data.notice, 'muted'));
    const download = node('a', 'Download checkpoint'); download.href = `/${runId}/sessions/${String(data.session).padStart(3, '0')}/workspace.tar`;
    card.append(download); $('checkpoints').append(card); checkpoints.set(data.session, true);
  }
  $('close-preview').addEventListener('click', () => $('preview').close());
  $('stop').addEventListener('click', async () => {
    $('stop').disabled = true;
    try { await api(`/api/run/${runId}/stop`, {}); $('stop').textContent = 'Stopping…'; }
    catch (err) { error(err.message); }
  });
  function renderStatus() {
    if (!status) return;
    const r = status.recipe, result = status.result;
    $('run-title').textContent = status.name;
    $('run-id').textContent = `${status.kind} · ${status.id} · ${r.model.think} thinking · ${number(r.model.context_tokens)} context`;
    $('phase').textContent = status.active ? (['starting', 'stopping', 'finalizing'].includes(status.state) ? status.state : phase) : result?.stop_reason || status.state;
    $('session').textContent = `${session || '—'} / ${r.sessions}`;
    const elapsed = result?.elapsed_seconds ?? (Date.now() - (started || Date.parse(status.created))) / 1000;
    $('time').textContent = duration(status.active ? r.limits.wall_seconds - elapsed : elapsed);
    $('time-label').textContent = status.active ? 'Time remaining' : 'Elapsed';
    if (result) { generated = result.generated_tokens; charged = result.token_budget_charged; pending = 0; }
    $('tokens').textContent = `${number(generated)} / ${number(r.limits.generated_tokens)}`;
    const unknown = Math.max(0, (charged || generated) - generated) + (!status.active ? pending : 0);
    $('token-detail').textContent = `Confirmed${unknown ? ` · ${number(unknown)} reserved, usage unknown` : ''}${pending && status.active ? ` · up to ${number(pending)} in flight` : ''}`;
    $('activity').textContent = lastChunk ? `Last model chunk ${duration((Date.now() - lastChunk) / 1000)} ago` : 'No model chunks yet';
    $('stop').disabled = !status.can_stop || ['stopping', 'finalizing'].includes(status.state);
    $('stop').textContent = status.state === 'stopping' ? 'Stopping…' : 'Stop run';
    const failure = [status.error, result?.error, ...(result?.cleanup_errors || [])].filter(Boolean).join('\n');
    $('run-error').textContent = failure; $('run-error').hidden = !failure;
    if (status.report_url) { $('report-link').href = status.report_url; $('report-link').hidden = false; }
    if (!status.active) { $('waiting').hidden = true; following = false; followState(); }
  }
  async function refreshHealth() {
    if (Date.now() - lastHealth < 10000) return;
    lastHealth = Date.now();
    const health = await api(`/api/run/${runId}/health`);
    $('health').textContent = health.label; $('health').className = ['cpu', 'unavailable'].includes(health.state) ? 'error' : '';
    $('health-detail').textContent = health.context ? `${number(health.context)} loaded context · current device status` : 'Current device status';
    $('health').title = health.error || '';
  }
  async function poll() {
    let delay = 1000;
    try {
      const data = await api(`/api/run/${runId}/events?cursor=${cursor}`);
      status = data.status;
      for (const event of data.events) record(event);
      cursor = data.cursor;
      for (const turn of dirty) {
        turn.thinking.lastChild.textContent = short(turn.thought, 100000);
        turn.thinking.hidden = !turn.thought;
        turn.content.textContent = short(turn.text, 100000); turn.content.hidden = !turn.text;
      }
      dirty.clear(); renderStatus(); connected();
      if (data.more) delay = 30;
      else {
        await loadCheckpoint(); await refreshHealth(); scrollToLive();
        if (!status.active && [...checkpoints.values()].every(Boolean)) return;
      }
    } catch (err) { $('connection').textContent = 'Reconnecting…'; error(err.message, true); delay = 2000; }
    setTimeout(poll, delay);
  }
  async function init() {
    if (runId) { $('watch').hidden = false; poll(); return; }
    $('launcher').hidden = false; $('runs-panel').hidden = false;
    try {
      const data = await api('/api/bootstrap'); token = data.token;
      for (const recipe of data.recipes) {
        const option = node('option', recipe.error ? `${recipe.path} · invalid recipe` : `${recipe.path} · ${recipe.name}`);
        option.value = recipe.path; option.disabled = Boolean(recipe.error); $('recipe').append(option);
      }
      if (data.recipes.some(recipe => !recipe.error)) await loadPreview();
      else error('No valid experiment recipes are available.');
    } catch (err) { error(err.message); }
    refreshRuns();
  }
  init();
})();
