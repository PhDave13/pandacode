import './remote.css';
import { escape } from './dom';
import { remoteComputers, type RemoteDevice } from './remote-types';

/** The sidebar's Computers rows: online host devices other than this one. Empty when there are none. */
export function computersHtml(devices: RemoteDevice[]): string {
  const rows = remoteComputers(devices);
  if (!rows.length) return '';
  return `<div class="sidebar-computers-head" id="computers-label">Computers</div><div role="group" aria-labelledby="computers-label">${rows.map(d => `<button type="button" class="sidebar-computer" data-action="open-computer" data-device="${escape(d.id)}" title="Open ${escape(d.name)}"><span class="remote-dot" data-online="true" aria-hidden="true"></span>${escape(d.name)}</button>`).join('')}</div>`;
}
