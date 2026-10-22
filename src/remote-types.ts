/* Shapes of the PandaCode Connect commands and events (docs/remote-access.md). */
export interface RemoteStatus {
  configured: boolean; signedIn: boolean; account?: { login: string; avatarUrl?: string };
  device: { id: string; name: string; fingerprint: string }; hostEnabled: boolean; connected: boolean;
}
export interface RemoteDevice { id: string; name: string; platform: string; fingerprint: string; host: boolean; online: boolean; trusted: boolean; lastSeen: number; current: boolean }
export interface RemoteSignIn { userCode: string; verificationUri: string; expiresIn: number }
export interface RemoteHost {
  call<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  listen(name: string, callback: (event: any) => void): Promise<() => void>;
  notify(text: string): void;
  openUrl(url: string): Promise<void>;
}
/** Computers this one can open: online hosts other than itself. */
export const remoteComputers = (devices: RemoteDevice[]): RemoteDevice[] => devices.filter(device => device.host && device.online && !device.current);
