/** Browser preview for PandaCode Connect: in-memory sample account and computers, no network. Native builds use src-tauri/src/remote. */
import type { RemoteDevice, RemoteSignIn, RemoteStatus } from './remote-types';

interface Task { live?: string; sessionId: string; title: string; projectId: string; status: string; version: number; older?: { id: number; role: string; content: string; createdAt: number }[]; messages: { id: number; role: string; content: string; createdAt: number }[] }
const THIS = 'dev-this';
let signedIn = false, hostEnabled = false, nextId = 100, nextTask = 3;
let devices: RemoteDevice[] = [], focus = '', cancelSignIn: (() => void) | undefined;
const offline = new Set<string>();
const trusted = new Set<string>(['dev-studio']);
const HOSTILE = '"><img src=x onerror="window.__xss=1">';
const tasks: Task[] = [
  { sessionId: 'remote-1', title: 'Fix the flaky export test', projectId: 'p1', status: 'idle', version: 1, older: [{ id: 0, role: 'user', content: 'Earlier: the export test is flaky.', createdAt: Date.now() - 700000 }], messages: [
    { id: 1, role: 'user', content: 'The export test fails about one run in five. Can you find out why?', createdAt: Date.now() - 600000 },
    { id: 2, role: 'assistant', content: 'The test races the file watcher. I **added a wait** for the write to settle and it now passes ten runs in a row.', createdAt: Date.now() - 590000 },
  ] },
  { sessionId: 'remote-2', title: 'Write release notes', projectId: 'p1', status: 'idle', version: 1, messages: [
    { id: 3, role: HOSTILE, content: 'role payload', createdAt: Date.now() - 5000 },
    { id: 4, role: 'user', content: `<img src=x onerror="window.__xss=1"> plain text`, createdAt: Date.now() - 4000 },
  ] },
];
const status = (): RemoteStatus => ({ configured: true, signedIn, account: signedIn ? { login: 'octocat' } : undefined, device: { id: THIS, name: 'This Mac', fingerprint: '3fa91c07d2b64e58' }, hostEnabled, connected: signedIn });
const sampleDevices = (): RemoteDevice[] => [
  { id: THIS, name: 'This Mac', platform: 'macOS', fingerprint: '3fa91c07d2b64e58', host: hostEnabled, online: true, trusted: true, lastSeen: Date.now(), current: true },
  { id: 'dev-studio', name: 'Studio iMac', platform: 'macOS', fingerprint: '9b02e4aa71c3d605', host: true, online: !offline.has('dev-studio'), trusted: trusted.has('dev-studio'), lastSeen: Date.now(), current: false },
  { id: 'dev-nas', name: 'Home NAS', platform: 'Linux', fingerprint: '17aa5e90b3c4d286', host: true, online: true, trusted: trusted.has('dev-nas'), lastSeen: Date.now(), current: false },
  { id: 'dev-laptop', name: 'Old laptop', platform: 'Windows', fingerprint: 'c4d81f3e0a9957b2', host: true, online: false, trusted: trusted.has('dev-laptop'), lastSeen: Date.now() - 86_400_000, current: false },
];
const summary = (t: Task) => ({ id: t.sessionId, sessionId: t.sessionId, kind: 'task', title: t.title, projectId: t.projectId, projectName: 'orbit', provider: 'claude', status: t.status, updatedAt: Date.now() });
const find = (id: unknown) => { const t = tasks.find(task => task.sessionId === id); if (!t) throw new Error('Task not found'); return t; };

export function invokeRemote<T>(command: string, args: Record<string, any>, emit: (name: string, payload: unknown) => void): Promise<T> {
  return Promise.resolve().then(() => handle(command, args, emit) as T);
}
function handle(command: string, args: Record<string, any>, emit: (name: string, payload: unknown) => void): unknown {
  switch (command) {
    case 'remote_status': (window as any).__remoteOnline = (id: string, online: boolean) => setPreviewOnline(id, online, emit); return status();
    case 'remote_sign_in_start': return { userCode: 'WDJB-MJHT', verificationUri: 'https://github.com/login/device', expiresIn: 900 } satisfies RemoteSignIn;
    case 'remote_sign_in_finish': return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { cancelSignIn = undefined; signedIn = true; devices = sampleDevices(); emit('remote://status', status()); resolve(status()); }, (window as any).__signInDelay ?? 400);
      cancelSignIn = () => { clearTimeout(timer); reject(new Error('Sign-in cancelled')); };
    });
    case 'remote_sign_in_cancel': cancelSignIn?.(); cancelSignIn = undefined; return {};
    case 'remote_sign_out': signedIn = false; hostEnabled = false; devices = []; emit('remote://status', status()); return status();
    case 'remote_set_host_enabled': hostEnabled = Boolean(args.enabled); devices = sampleDevices(); emit('remote://presence', devices); return status();
    case 'remote_devices': return devices = signedIn ? sampleDevices() : [];
    case 'remote_set_trusted': if (args.trusted) trusted.add(args.deviceId); else trusted.delete(args.deviceId); return devices = sampleDevices();
    case 'remote_revoke_device': devices = devices.filter(d => d.id !== args.deviceId); emit('remote://presence', devices); return {};
    case 'remote_call': return remoteCall(args, emit);
    default: throw new Error(`Unknown preview command: ${command}`);
  }
}
function remoteCall(args: Record<string, any>, emit: (name: string, payload: unknown) => void): unknown {
  const device = devices.find(d => d.id === args.deviceId);
  if (!device?.online) throw new Error('That computer is offline');
  if (!trusted.has(device.id)) throw new Error(`${device.name} isn't trusted on this computer yet. Compare its fingerprint and trust it in Settings → Remote access.`);
  const p = args.params ?? {};
  const changed = (sessionId: string, live?: { liveText: string; status: string; historyVersion: number }) => emit('remote://task-changed', { deviceId: device.id, sessionId, ...(live && sessionId === focus ? { live } : {}) });
  switch (args.method) {
    case 'subscribe': focus = String(p.sessionId ?? ''); return {};
    case 'tasks.list': return { tasks: tasks.map(summary), projects: [{ id: 'p1', name: 'orbit', path: '/Users/you/orbit' }], context: {}, activeCount: 0, teamWorkerCount: 0 };
    case 'task.get': {
      const t = find(p.sessionId), win = window as any;
      win.__remoteGets = (win.__remoteGets ?? 0) + 1;
      const answer = () => {
        if (p.beforeId !== undefined) return { messages: t.older ?? [], hasOlder: false, liveText: '', status: t.status, historyVersion: t.version, historyUnchanged: false };
        const same = p.historyVersion === t.version;
        return { messages: same ? [] : t.messages, hasOlder: Boolean(t.older), liveText: t.live ?? '', status: t.status, provider: 'claude', historyVersion: t.version, historyUnchanged: same };
      };
      return t.sessionId === 'remote-2' ? new Promise(resolve => setTimeout(() => resolve(answer()), 300)) : answer();
    }
    case 'task.start': {
      const t: Task = { sessionId: `remote-${nextTask++}`, title: String(p.text).slice(0, 40), projectId: String(p.projectId ?? 'p1'), status: 'idle', version: 1, messages: [] };
      tasks.unshift(t); reply(t, String(p.text), changed); return { sessionId: t.sessionId };
    }
    case 'task.send': { const t = find(p.sessionId), queued = t.status === 'running'; reply(t, String(p.text), changed); return { queued }; }
    case 'task.stop': find(p.sessionId).status = 'idle'; return {};
    case 'approval.respond': return {};
    default: throw new Error(`Unknown remote method: ${args.method}`);
  }
}
type Changed = (id: string, live?: { liveText: string; status: string; historyVersion: number }) => void;
function reply(t: Task, text: string, changed: Changed): void {
  const live = (liveText: string) => { t.live = liveText; changed(t.sessionId, { liveText, status: 'running', historyVersion: t.version }); };
  t.messages.push({ id: nextId++, role: 'user', content: text, createdAt: Date.now() }); t.version++; t.status = 'running'; live('');
  setTimeout(() => live('Working on it'), 300);
  setTimeout(() => live('Working on it, almost there'), 700);
  setTimeout(() => {
    t.messages.push({ id: nextId++, role: 'assistant', content: `Done: ${text}`, createdAt: Date.now() }); t.version++; t.status = 'idle'; t.live = '';
    changed(t.sessionId, { liveText: '', status: 'idle', historyVersion: t.version });
  }, 1100);
}
/** Test hook: a computer going offline or coming back. */
export function setPreviewOnline(id: string, online: boolean, emit: (name: string, payload: unknown) => void): void {
  if (online) offline.delete(id); else offline.add(id);
  devices = sampleDevices(); emit('remote://presence', devices);
}
