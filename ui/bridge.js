/* The page's only door to the Rust side: window.qazi */
(() => {
'use strict';
const { invoke } = window.__TAURI__.core, { listen } = window.__TAURI__.event;
const on = (ev) => (cb) => { listen(ev, (e) => cb(e.payload)); };
const quiet = (p) => p.catch(() => {});

// dropped files come without paths; the host turns the file objects back into paths
let dropSeq = 0;
const dropId = Math.random().toString(36).slice(2, 6);
const dropWait = new Map();
listen('drop-paths', (e) => { const done = dropWait.get(e.payload.token); if (done) { dropWait.delete(e.payload.token); done(e.payload.paths || []); } });

window.qazi = {
  onCursor: on('cursor'), onHelper: on('helper'), onSettings: on('settings'), onClip: on('clip'), onDownload: on('download'),
  onShelf: on('shelf'), onShelfDragging: on('shelf-dragging'), onClips: on('clips'), onLevels: on('levels'),
  onPress: on('press'), onRelease: on('release'), onOpen: on('open'), onHotkey: on('hotkey'), onNotes: on('notes'), onVault: on('vault'), onOcr: on('ocr'), onOcrRead: on('ocr-read'), onSpeedo: on('speedo'),
  onMonitorState: on('monitor-state'), onDragFx: on('drag-fx'), onPillLanded: on('pill-landed'), onMergePull: on('merge-pull'), onMergeComplete: on('merge-complete'),

  setInteractive: (v) => quiet(invoke('set_interactive', { on: !!v })),
  setPillWidth: (w) => quiet(invoke('set_pill_width', { w: Number(w) || 0 })),
  setStackOpen: (open) => quiet(invoke('set_stack_open', { open: !!open })),
  setTyping: (v, restore) => invoke('set_typing', { on: !!v, restore: !!restore }).catch(() => ({})),
  lostClick: (info) => quiet(invoke('lost_click', { info: String(info) })),
  pillDragStart: () => quiet(invoke('pill_drag_start')),
  switchMonitor: () => quiet(invoke('switch_monitor')),
  getMonitorState: () => invoke('monitor_state').catch(() => null),
  toggleDual: () => quiet(invoke('toggle_dual')),

  telemetryGet: () => invoke('telemetry_get').catch(() => ({})),
  watchdogScan: () => invoke('watchdog_scan').catch(() => []),
  watchdogClean: () => invoke('watchdog_clean').catch(() => ({ ok: false })),
  watchdogKill: (pid) => invoke('watchdog_kill', { pid: Number(pid) }).catch(() => false),
  agyHubGet: () => invoke('agy_hub_get').catch(() => ({ convs: [], artifacts: [] })),
  agyFocus: () => invoke('agy_focus').catch(() => false),
  agyOpen: (path, reveal) => quiet(invoke('agy_open', { path: String(path || ''), reveal: !!reveal })),
  persianFix: (mode, text) => invoke('persian_fix', { mode: String(mode), text: String(text || '') }).catch(() => ({ text: String(text || ''), lang: '' })),

  media: (cmd) => quiet(invoke('media', { cmd })),
  seek: (ms) => quiet(invoke('seek', { ms })),
  setVolume: (v) => quiet(invoke('set_volume', { v })),
  toggleMute: () => quiet(invoke('toggle_mute')),
  setViz: (v) => quiet(invoke('set_viz', { on: !!v })),

  activate: (id, aumid) => quiet(invoke('notif_act', { id: String(id), aumid: aumid || '' })),
  dismiss: (id) => quiet(invoke('notif_dismiss', { id: String(id) })),
  clearNotifs: () => quiet(invoke('notif_clear')),
  hintShown: () => quiet(invoke('settings_set', { key: 'dndHint', value: true })),

  pathsFor(files) {
    return new Promise((resolve) => {
      const list = [...files];
      if (!list.length || !(window.chrome && chrome.webview && chrome.webview.postMessageWithAdditionalObjects)) { resolve(list.map(() => '')); return; }
      const token = dropId + '-' + (++dropSeq);
      dropWait.set(token, resolve);
      setTimeout(() => { if (dropWait.delete(token)) resolve(list.map(() => '')); }, 1500);
      try { chrome.webview.postMessageWithAdditionalObjects('island-drop:' + token, list); } catch { dropWait.delete(token); resolve(list.map(() => '')); }
    });
  },
  shelfAdd: (paths) => invoke('shelf_add', { paths }),
  shelfAddData: (name, type, buf) => invoke('shelf_add_data', new Uint8Array(buf), { headers: { 'x-name': encodeURIComponent(name || ''), 'x-type': encodeURIComponent(type || '') } }),
  shelfAddUrl: (url) => invoke('shelf_add_url', { url }),
  shelfGet: () => invoke('shelf_get'),
  shelfRemove: (path) => quiet(invoke('shelf_remove', { path })),
  shelfClear: () => quiet(invoke('shelf_clear')),
  shelfReveal: (path) => quiet(invoke('shelf_reveal', { path })),
  shelfDrag: (paths) => quiet(invoke('shelf_drag', { paths })),
  openDownload: (path) => quiet(invoke('open_download', { path })),

  clipsGet: () => invoke('clips_get'),
  clipCopy: (id) => quiet(invoke('clip_copy', { id })),
  clipPin: (id) => quiet(invoke('clip_pin', { id })),
  clipRemove: (id) => quiet(invoke('clip_remove', { id })),
  clipsClear: () => quiet(invoke('clips_clear')),
  copy: (text) => quiet(invoke('copy_text', { text })),

  getSettings: () => invoke('settings_get'),
  setSetting: (key, value) => quiet(invoke('settings_set', { key, value })),
  setCalendar: (value) => quiet(invoke('settings_set', { key: 'calendar', value })),
  action: (name) => quiet(invoke('action', { name })),
  ocrDone: (text, conf) => quiet(invoke('ocr_done', { text: String(text || ''), conf: Number(conf) || 0 })),
  speedo: (cmd, more) => invoke('speedo_cmd', { cmd, ...(more || {}) }).catch(() => ({ ok: false })),
  updateCheck: () => invoke('update_check').catch(() => 'failed'),
  updateInstall: () => invoke('update_install').catch(() => false),
  openUrl: (url) => quiet(invoke('open_url', { url })),

  notesGet: () => invoke('notes_get'),
  noteSave: (id, text, remind) => invoke('note_save', { id: id || null, text, remind: remind || 0 }),
  notePin: (id) => invoke('note_pin', { id }),
  noteFired: (id) => invoke('note_fired', { id }),
  noteRemove: (id) => invoke('note_remove', { id }),

  vaultList: () => invoke('vault_list'),
  vaultGenerate: (len, symbols) => invoke('vault_generate', { len, symbols: !!symbols }),
  vaultSave: (site, user, pass) => invoke('vault_save', { site, user, pass }).catch(() => 0),     // the new entry's id, 0 = not saved
  vaultEdit: (id, site, user) => invoke('vault_edit', { id, site, user }).catch(() => false),
  fgContext: () => invoke('fg_context').catch(() => ({})),
  vaultCopy: (id, what) => invoke('vault_copy', { id, what }),
  copySecret: (text) => invoke('copy_secret', { text }),
  vaultReveal: (id) => invoke('vault_reveal', { id }),
  vaultRemove: (id) => invoke('vault_remove', { id }),

  lyricsGet: (key, title, artist, durMs, force) => quiet(invoke('lyrics_get', { key, title, artist, durMs: Number(durMs) || 0, force: !!force })),
  lyricsSearch: (query, durMs) => invoke('lyrics_search', { query, durMs: Number(durMs) || 0 }).catch(() => []),
  lyricsPick: (key, id) => invoke('lyrics_pick', { key, id: Number(id) || 0 }).catch(() => false),
  lyricsOffset: (key, offsetMs) => invoke('lyrics_offset', { key, offsetMs: Math.round(Number(offsetMs) || 0) }).catch(() => false),

  audioOutputs: () => invoke('audio_outputs').catch(() => []),
  audioSetOutput: (id) => invoke('audio_set_output', { id }).catch(() => false),
  audioCycleOutput: () => invoke('audio_cycle_output').catch(() => ({ ok: false, devices: [] })),
};
window.addEventListener('error', (e) => quiet(invoke('debug_out', { text: '[page error] ' + e.message + ' @' + e.lineno })));
})();
