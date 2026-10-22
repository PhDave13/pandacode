/* The remote computer view: another computer's tasks, a task's transcript, and a composer,
 * all through `remote_call` (end-to-end encrypted by the relay). */
import { escape, icon, markdownHtml, morph } from './dom';
import type { RemoteDevice, RemoteHost } from './remote-types';

interface TaskSummary { id: string; sessionId: string; kind: string; title: string; projectId: string; projectName: string; status: string; updatedAt: number }
interface Snapshot { tasks: TaskSummary[]; projects: { id: string; name: string }[] }
interface Msg { id: number; role: string; content: string; createdAt: number }
interface Approval { requestId: string; title?: string; detail?: string }
/** `approvals` is optional on the wire: a host that predates it only reports status "waiting". */
interface Thread { messages: Msg[]; liveText: string; status: string; approvals?: Approval[]; hasOlder?: boolean; historyVersion?: number; historyUnchanged?: boolean }
interface Live { liveText: string; status: string; historyVersion: number }

export interface RemoteView { open(device: RemoteDevice): void; presence(devices: RemoteDevice[]): void; close(): void }
const RESUBSCRIBE = 9 * 60_000, POLL = 30_000, LIST_THROTTLE = 1000, CACHE_LIMIT = 400;
const LABEL: Record<string, string> = { running: 'Working', waiting: 'Needs approval', coordinating: 'Coordinating', idle: 'Ready', completed: 'Completed', error: 'Failed', stopped: 'Stopped' };

/** Maps a failed call to a sentence that says what to do. */
export function remoteError(e: unknown): string {
  const text = e instanceof Error ? e.message : String(e);
  if (/remote control is off/i.test(text)) return 'Remote control is off on that computer. Turn on “Allow remote control of this computer” in its Settings → Remote access.';
  if (/time(d)? ?out/i.test(text)) return 'That computer didn’t answer in time. Check that PandaCode is running there, then try again.';
  if (/offline|undeliverable|not connected|unavailable/i.test(text)) return 'That computer is offline. It will reappear when PandaCode is running there.';
  return text || 'Something went wrong talking to that computer.';
}

/** The host is untrusted: only these roles reach an attribute, and every other string is escaped or sanitized Markdown. */
const ROLES = ['user', 'assistant', 'system'];
const UNTRUSTED = /isn.t trusted/i;
const text = (value: unknown) => typeof value === 'string' ? value : '';
const bubble = (role: string, inner: string) => `<div class="remote-msg" data-role="${ROLES.includes(role) ? role : 'system'}">${inner}</div>`;

export function mountRemote(root: HTMLElement, host: RemoteHost): RemoteView {
  let device: RemoteDevice | undefined, snapshot: Snapshot | undefined, thread: Thread | undefined;
  let selected = '', error = '', threadError = '', draft = '', sending = false, projectId = '';
  let earlier: Msg[] = [], earlierMore: boolean | undefined, earlierBusy = false, prepending = false;
  let active = false, connected = true, listenGen = 0, offs: (() => void)[] = [];
  let seq = 0, flight = 0, trailing = false, stale = 0;
  let timer = 0, poll = 0, listRefresh = 0;
  const rendered = new Map<string, string>();
  const call = <T>(method: string, params: Record<string, unknown> = {}) => host.call<T>('remote_call', { deviceId: device!.id, method, params });

  root.innerHTML = '<div class="remote-shell"><aside class="remote-list" aria-label="Tasks"></aside><section class="remote-main" aria-label="Remote task"></section></div>';
  const list = root.querySelector<HTMLElement>('.remote-list')!, main = root.querySelector<HTMLElement>('.remote-main')!;

  const tasks = () => (snapshot?.tasks ?? []).filter(task => task.kind !== 'subagent');
  function renderList(): void {
    if (!device) return;
    const rows = tasks().map(task => `<button type="button" class="remote-task" data-task="${escape(task.sessionId)}" ${task.sessionId === selected ? 'aria-current="true"' : ''}><span class="remote-task-title">${escape(task.title || 'Untitled')}</span><small>${escape(task.projectName)} · ${escape(LABEL[task.status] ?? task.status)}</small></button>`).join('');
    morph(list, `<header class="remote-list-head"><h2><span class="remote-dot" data-online="${device.online}" role="img" aria-label="${device.online ? 'Online' : 'Offline'}"></span>${escape(device.name)}</h2><button type="button" class="icon-button" data-new aria-label="New task" title="New task">${icon('plus')}</button></header>${error ? '' : rows || (snapshot ? '<p class="remote-note">No tasks on this computer yet.</p>' : '<p class="remote-note" role="status">Loading tasks…</p>')}`);
  }
  /** Rendered bubbles by role and source, so a streaming delta re-parses only the live text. */
  async function cached(role: string, content: string): Promise<string> {
    const key = `${role}\0${content}`;
    let html = rendered.get(key);
    if (html === undefined) {
      html = bubble(role, role === 'assistant' ? `<div class="markdown">${await markdownHtml(content)}</div>` : `<p>${escape(content)}</p>`);
      if (rendered.size >= CACHE_LIMIT) rendered.clear();
      rendered.set(key, html);
    }
    return html;
  }
  const hasOlder = () => earlierMore ?? Boolean(thread?.hasOlder);
  function messages(): Msg[] {
    const fresh = Array.isArray(thread?.messages) ? thread!.messages : [];
    const first = fresh[0]?.id;
    return [...(first === undefined ? earlier : earlier.filter(m => m.id < first)), ...fresh];
  }
  async function transcript(): Promise<string> {
    if (!thread) return '';
    const shown = messages().filter(m => ROLES.includes(m?.role) && text(m.content).trim() && !text(m.content).startsWith('{"type":'));
    const parts = await Promise.all(shown.map(m => cached(m.role, text(m.content))));
    if (text(thread.liveText).trim()) parts.push(await cached('assistant', text(thread.liveText)));
    const more = hasOlder() ? `<button type="button" class="secondary-button remote-earlier" data-earlier ${earlierBusy ? 'disabled' : ''}>Load earlier</button>` : '';
    return more + parts.join('');
  }
  function approvals(): string {
    if (!thread || thread.status !== 'waiting') return '';
    if (!thread.approvals?.length) return '<p class="remote-note" role="status">This task is waiting for approval on that computer.</p>';
    return thread.approvals.map(a => `<div class="remote-approval" role="group" aria-label="Approval requested"><div><strong>${escape(a.title ?? 'Approval requested')}</strong>${a.detail ? `<small>${escape(a.detail)}</small>` : ''}</div><button type="button" class="secondary-button" data-deny="${escape(a.requestId)}">Deny</button><button type="button" class="primary-button" data-approve="${escape(a.requestId)}">Approve</button></div>`).join('');
  }
  function composer(): string {
    const fresh = !selected, running = thread && (thread.status === 'running' || thread.status === 'waiting');
    const projects = snapshot?.projects ?? [];
    const pick = fresh && projects.length > 1 ? `<select data-project aria-label="Project">${projects.map(p => `<option value="${escape(p.id)}" ${p.id === projectId ? 'selected' : ''}>${escape(p.name)}</option>`).join('')}</select>` : '';
    return `<form class="remote-composer">${pick}<textarea rows="2" aria-label="${fresh ? 'New task prompt' : 'Message'}" placeholder="${fresh ? 'Start a task on ' + escape(device?.name ?? 'this computer') + '…' : running ? 'Queue a message…' : 'Message the agent…'}">${escape(draft)}</textarea><div class="remote-composer-actions">${running ? `<button type="button" class="secondary-button" data-stop>${icon('stop')}Stop</button>` : ''}<button type="submit" class="primary-button" ${sending ? 'disabled' : ''}>${fresh ? 'Start task' : running ? 'Queue' : 'Send'}</button></div></form>`;
  }
  /** Both computers must trust each other; the message says which side is missing. */
  function trustState(): string {
    const name = escape(device!.name), here = /on this computer/i.test(error);
    return `<h3>${here ? `This computer doesn’t trust ${name} yet` : `${name} doesn’t trust this computer yet`}</h3><p>${escape(error)}</p><p>Both computers must trust each other. Compare the key fingerprints in Settings → Remote access on each, then turn on “Trusted” for the other on both.</p>`;
  }
  let composerKey = '';
  async function renderMain(): Promise<void> {
    if (!device || !active) return;
    const stamp = ++stale;
    if (error) {
      main.innerHTML = `<div class="remote-state" role="alert">${UNTRUSTED.test(error) ? trustState() : `<p>${escape(error)}</p>`}<button type="button" class="secondary-button" data-retry>Try again</button></div>`;
      composerKey = '';
      return;
    }
    if (!main.querySelector('.remote-transcript')) main.innerHTML = '<div class="remote-banner-slot"></div><div class="remote-transcript" role="log" aria-label="Conversation"></div><div class="remote-approvals"></div><div class="remote-composer-slot"></div>';
    const log = main.querySelector<HTMLElement>('.remote-transcript')!;
    const body = !selected ? '<p class="remote-note">Describe a task to start it on this computer.</p>' : threadError ? `<p class="remote-error" role="alert">${escape(threadError)}</p>` : thread ? await transcript() : '<p class="remote-note" role="status">Loading conversation…</p>';
    if (stamp !== stale || !active) return;
    const atEnd = !prepending && log.scrollHeight - log.scrollTop - log.clientHeight < 40, height = log.scrollHeight;
    morph(main.querySelector('.remote-banner-slot')!, device.online ? '' : `<p class="remote-banner" role="status">${escape(device.name)} is offline. This conversation and your draft are kept; it reconnects when PandaCode is running there.</p>`);
    morph(log, body, 6);
    if (prepending) log.scrollTop += log.scrollHeight - height; else if (atEnd) log.scrollTop = log.scrollHeight;
    morph(main.querySelector('.remote-approvals')!, approvals());
    // The composer is rebuilt only when its shape changes; focus and caret survive the rebuild.
    const key = [selected, thread?.status === 'running' || thread?.status === 'waiting', sending, projectId, (snapshot?.projects ?? []).map(p => p.id).join(',')].join('|');
    if (key !== composerKey) {
      composerKey = key;
      const slot = main.querySelector<HTMLElement>('.remote-composer-slot')!, old = slot.querySelector('textarea');
      const focused = old !== null && document.activeElement === old, start = old?.selectionStart ?? 0, end = old?.selectionEnd ?? 0;
      slot.innerHTML = composer();
      const field = slot.querySelector('textarea');
      if (focused && field) { field.focus({ preventScroll: true }); field.setSelectionRange(start, end); }
    }
  }
  async function guarded<T>(work: () => Promise<T>, onError: (message: string) => void): Promise<T | undefined> {
    try { return await work(); } catch (e) {
      const message = remoteError(e);
      // Not trusted is a state of the whole computer, not a one-off failure: it replaces the view.
      if (UNTRUSTED.test(message)) { error = message; renderList(); void renderMain(); } else onError(message);
      return undefined;
    }
  }
  async function loadList(): Promise<void> {
    const mine = device;
    // A failure keeps an already loaded list on screen; only a first load shows the error.
    const next = await guarded(() => call<Snapshot>('tasks.list'), message => { if (device === mine && !snapshot) error = message; });
    if (device !== mine || !active) return;
    if (next) {
      snapshot = next; error = '';
      if (!next.projects.some(p => p.id === projectId)) projectId = next.projects[0]?.id ?? '';
    }
    renderList(); void renderMain();
  }
  const keepOnly = (latest: Thread, current: Thread | undefined): Thread => current ? { ...current, liveText: latest.liveText, status: latest.status, approvals: latest.approvals } : latest;
  /** One fetch at a time per selection, plus one trailing; replies for an earlier selection are dropped. */
  async function loadThread(): Promise<void> {
    if (!selected || !active) return;
    if (flight === seq) { trailing = true; return; }
    const token = flight = seq, id = selected, version = thread?.historyVersion;
    const next = await guarded(() => call<Thread>('task.get', { sessionId: id, ...(version === undefined ? {} : { historyVersion: version }) }), message => { if (token === seq) threadError = message; });
    if (flight === token) flight = 0;
    if (token !== seq || !active) return;
    if (next) {
      thread = next.historyUnchanged ? keepOnly(next, thread) : next; threadError = '';
    }
    void renderMain();
    if (trailing) { trailing = false; void loadThread(); }
  }
  async function loadEarlier(): Promise<void> {
    const first = messages()[0];
    if (!selected || !first || earlierBusy) return;
    const token = seq, id = selected;
    earlierBusy = true; void renderMain();
    const next = await guarded(() => call<Thread>('task.get', { sessionId: id, beforeId: first.id }), host.notify);
    earlierBusy = false;
    if (token !== seq || !active) return;
    if (next && Array.isArray(next.messages)) { earlier = [...next.messages, ...earlier]; earlierMore = Boolean(next.hasOlder); }
    prepending = true; await renderMain(); prepending = false;
  }
  const subscribeHost = () => void guarded(() => call('subscribe', selected ? { sessionId: selected } : {}), () => undefined);
  function select(id: string): void {
    seq++; trailing = false; selected = id; thread = undefined; threadError = ''; earlier = []; earlierMore = undefined; earlierBusy = false;
    subscribeHost(); renderList(); void renderMain(); void loadThread();
  }
  function newTask(): void {
    seq++; trailing = false; selected = ''; thread = undefined; threadError = ''; earlier = []; earlierMore = undefined;
    subscribeHost(); renderList(); void renderMain();
  }

  async function submit(): Promise<void> {
    const field = main.querySelector<HTMLTextAreaElement>('textarea');
    const text = (field?.value ?? '').trim();
    if (!text || sending) return;
    sending = true; draft = '';
    if (field) field.value = '';
    const created = await guarded(async () => {
      if (selected) { await call('task.send', { sessionId: selected, text }); return selected; }
      const result = await call<{ sessionId: string }>('task.start', { ...(projectId ? { projectId } : {}), text });
      return result.sessionId;
    }, message => { draft = text; if (field) field.value = text; host.notify(message); });
    sending = false;
    if (created && created !== selected) { select(created); void loadList(); }
    else { void loadThread(); void renderMain(); }
  }

  main.addEventListener('submit', event => { event.preventDefault(); void submit(); });
  main.addEventListener('input', event => { if (event.target instanceof HTMLTextAreaElement) draft = event.target.value; });
  main.addEventListener('change', event => { if (event.target instanceof HTMLSelectElement) projectId = event.target.value; });
  main.addEventListener('keydown', event => {
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && event.target instanceof HTMLTextAreaElement) { event.preventDefault(); void submit(); }
  });
  main.addEventListener('click', event => {
    const button = (event.target as HTMLElement).closest<HTMLElement>('button');
    if (!button) return;
    const respond = (requestId: string, approve: boolean) => void guarded(() => call('approval.respond', { sessionId: selected, requestId, approve }), host.notify).then(() => loadThread());
    if (button.hasAttribute('data-retry')) { error = ''; snapshot = undefined; void loadList(); }
    else if (button.hasAttribute('data-earlier')) void loadEarlier();
    else if (button.hasAttribute('data-stop')) void guarded(() => call('task.stop', { sessionId: selected }), host.notify).then(() => loadThread());
    else if (button.dataset.approve) respond(button.dataset.approve, true);
    else if (button.dataset.deny) respond(button.dataset.deny, false);
  });
  list.addEventListener('click', event => {
    const button = (event.target as HTMLElement).closest<HTMLElement>('button');
    if (button?.dataset.task) select(button.dataset.task);
    else if (button?.hasAttribute('data-new')) newTask();
  });

  function onTaskChanged(event: { deviceId: string; sessionId: string; live?: Live }): void {
    if (!active || !device || event?.deviceId !== device.id) return;
    if (event.sessionId === selected) {
      const live = event.live;
      if (live && thread && live.historyVersion === thread.historyVersion) {
        // History is unchanged: only the streaming text and status moved, so nothing is fetched.
        const moved = live.status !== thread.status;
        thread = { ...thread, liveText: text(live.liveText), status: live.status };
        void renderMain();
        if (moved) void loadThread();
      } else void loadThread();
    }
    if (!listRefresh) listRefresh = window.setTimeout(() => { listRefresh = 0; void loadList(); }, LIST_THROTTLE);
  }
  function resync(): void { subscribeHost(); void loadList(); void loadThread(); }
  function listen(): void {
    const gen = ++listenGen;
    const keep = (pending: Promise<() => void>) => void pending.then(fn => { if (gen === listenGen && active) offs.push(fn); else fn(); }, () => undefined);
    keep(host.listen('remote://task-changed', onTaskChanged));
    keep(host.listen('remote://status', (status: { connected?: boolean }) => {
      const back = status?.connected === true && !connected;
      connected = status?.connected !== false;
      if (back && active) resync();
    }));
  }

  return {
    open(next) {
      const same = device?.id === next.id;
      device = next;
      if (!same) { snapshot = undefined; thread = undefined; selected = ''; error = ''; threadError = ''; draft = ''; projectId = ''; composerKey = ''; earlier = []; earlierMore = undefined; seq++; flight = 0; trailing = false; rendered.clear(); }
      if (active) { renderList(); void renderMain(); return; }
      active = true; connected = true; listen();
      subscribeHost();
      timer = window.setInterval(subscribeHost, RESUBSCRIBE);
      poll = window.setInterval(() => { if (!document.hidden) void loadList(); }, POLL);
      renderList(); void renderMain(); void loadList(); void loadThread();
    },
    presence(devices) {
      const next = devices.find(item => item.id === device?.id);
      if (!active || !device || !next) return;
      const back = next.online && !device.online;
      device = next;
      renderList(); void renderMain();
      if (back) resync();
    },
    close() {
      if (!active) return;
      active = false; seq++; flight = 0; trailing = false; listenGen++;
      window.clearInterval(timer); window.clearInterval(poll); window.clearTimeout(listRefresh); listRefresh = 0;
      for (const off of offs.splice(0)) off();
    },
  };
}
