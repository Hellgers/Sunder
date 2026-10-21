import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import vm from "node:vm";
import ts from "typescript";

function load(file, mocks = {}, extra = {}) {
 const source = fs.readFileSync(new URL(`../lib/${file}`, import.meta.url), "utf8");
 const code = ts.transpileModule(source, {compilerOptions: {module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022}}).outputText;
 const exports = {};
 vm.runInNewContext(code, {exports, require(name) { if (name in mocks) return mocks[name]; throw new Error(`Unexpected import ${name}`); }, console, setTimeout, clearTimeout, ...extra});
 return exports;
}
const tracks = load("util/tracks.ts");
const track = id => ({id, title: id, artist: "Artist", thumbnail: "", duration_secs: 100});

function playerModule(bridgeOverrides = {}) {
 const prefetch = [];
 const module = load("state/player.svelte.ts", {"../util/tracks": tracks, "../ipc/bridge": {prefetchTrack: async id => prefetch.push(id), setRepeatMode: async () => {}, stop: async () => {}, getTracksByIds: async ids => ids.map(track), ...bridgeOverrides}}, {
  $state: Object.assign(value => value, {snapshot: value => value}),
  $derived: Object.assign(value => value, {by: () => undefined}), $effect: {root: () => {}}, setInterval, clearInterval,
 });
 return {player: module.player, prefetch};
}
function bridgeModule(player, invoke, events = {}, dialog = {}) {
 return load("ipc/bridge.ts", {
  "../util/tracks": tracks, "@tauri-apps/api/core": {invoke},
  "@tauri-apps/api/event": {listen: async (name, fn) => { events[name] = fn; return () => {}; }},
  "@tauri-apps/api/app": {}, "@tauri-apps/plugin-dialog": dialog,
  "../state/player.svelte": {player}, "../state/config.svelte": {config: {current: {}}},
  "../state/lyrics.svelte": {lyricsState: {}}, "../state/downloads.svelte": {downloads: {}},
 });
}

test("playlist queue keeps repeated occurrences and skips unavailable rows in both directions", () => {
 const {player, prefetch} = playerModule();
 const list = [track("same"), track("unavailable:hidden"), track("same"), track("last")];
 player.setQueue(list);
 assert.equal(player.playFromQueue(0).id, "same");
 assert.equal(player.nextTrack().id, "same");
 assert.equal(player.queueIndex, 2);
 assert.equal(player.prevTrack().id, "same");
 assert.equal(player.queueIndex, 0);
 assert.equal(player.queue.length, 4);
 assert.ok(!prefetch.some(id => id.startsWith("unavailable:")));
 player.addToQueue(track("same"));
 assert.equal(player.queue.length, 5);
 player.playFromQueue(2);
 player.currentTrack = player.queue[2];
 player.playNext(track("insert"));
 assert.equal(player.queueIndex, 2);
 assert.equal(player.queue[3].id, "insert");
});

test("playing the second duplicate preserves its queue position; missing rows never invoke backend", async () => {
 const {player} = playerModule();
 player.setQueue([track("same"), track("other"), track("same")]);
 const selected = player.playFromQueue(2);
 const calls = [];
 const bridge = bridgeModule(player, async (name, args) => calls.push([name, args]));
 await bridge.playTrack(selected);
 assert.equal(player.queueIndex, 2);
 await bridge.playTrack(track("unavailable:hidden"));
 await bridge.prefetchTrack("unavailable:hidden");
 await bridge.downloadTrack("unavailable:hidden");
 assert.equal(calls.length, 1);
 assert.equal(calls[0][0], "play_track");
});

test("radio uses backend ordering, retains duplicates and does not destroy queue on request failure", async () => {
 const {player} = playerModule();
 const radio = [track("unavailable:gone"), track("first"), track("first"), track("tail")];
 const calls = [];
 const bridge = bridgeModule(player, async (name, args) => { calls.push([name, args]); return name === "ytmusic_radio" ? radio : undefined; });
 await bridge.startRadio(track("seed"));
 assert.deepEqual(Array.from(player.queue, t => t.id), radio.map(t => t.id));
 assert.equal(player.queueIndex, 1);
 assert.equal(calls[0][1].videoId, "seed");
 const failed = bridgeModule(player, async () => { throw new Error("Session expired"); });
 await assert.rejects(failed.startRadio(track("new")), /Session expired/);
 assert.equal(player.queueIndex, 1);
});

test("quality updates from another track cannot overwrite current stream metadata", async () => {
 const {player} = playerModule(); player.currentTrack = track("current");
 const events = {}; const bridge = bridgeModule(player, async () => {}, events);
 const cleanup = bridge.initProgressListener(); await Promise.resolve();
 events["playback-quality"]({payload: {video_id: "old", codec: "wrong"}});
 assert.equal(player.quality, null);
 events["playback-quality"]({payload: {video_id: "current", codec: "opus", source: "stream"}});
 assert.equal(player.quality.codec, "opus"); cleanup();
});

const {pollDeviceLogin} = load("util/deviceLogin.ts");
function fakeClock() {
 let now = 0; let next = null; let delay = null;
 return {now: () => now, set(fn, ms) {next = fn; delay = ms; return 1;}, clear() {next = null;},
  get delay() {return delay;}, async tick(ms) {now += ms; const fn = next; next = null; if (fn) await fn();}, hasTimer: () => next !== null};
}
const code = {interval: 5, expires_in: 100};
test("device login waits prescribed interval and backs off slow_down without parallel polling", async () => {
 const clock = fakeClock(); let calls = 0; let connected = false;
 pollDeviceLogin(code, async () => {calls++; return {status: calls === 1 ? "slow_down" : "connected", interval: 5};}, () => connected = true, assert.fail, clock);
 assert.equal(calls, 0); assert.equal(clock.delay, 5000);
 await clock.tick(5000); assert.equal(calls, 1); assert.equal(clock.delay, 10000);
 await clock.tick(10000); assert.equal(connected, true); assert.equal(clock.hasTimer(), false);
});
test("device login cancellation suppresses in-flight callbacks and errors terminate polling", async () => {
 const clock = fakeClock(); let resolve; let notified = false;
 const cancel = pollDeviceLogin(code, () => new Promise(r => resolve = r), () => notified = true, () => notified = true, clock);
 const pending = clock.tick(5000); cancel(); resolve({status: "connected", interval: 5}); await pending;
 assert.equal(notified, false); assert.equal(clock.hasTimer(), false);
 let error = ""; const failing = fakeClock();
 pollDeviceLogin(code, async () => {throw new Error("access_denied");}, assert.fail, e => error = e, failing);
 await failing.tick(5000); assert.match(error, /access_denied/); assert.equal(failing.hasTimer(), false);
});
test("expired device codes stop before requesting a token", async () => {
 const clock = fakeClock(); let calls = 0; let error = "";
 pollDeviceLogin({...code, expires_in: 2}, async () => {calls++;}, assert.fail, e => error = e, clock);
 assert.equal(clock.delay, 2000); await clock.tick(2000);
 assert.equal(calls, 0); assert.match(error, /expired/);
});


test("saving a queue sends all ordered occurrences and uses the persisted count", async () => {
 const {player} = playerModule();
 const queue = [track("A"), track("A"), track("unavailable:missing"), track("B")];
 player.setQueue(queue);
 let saved;
 const bridge = bridgeModule(player, async (name, args) => {
  assert.equal(name, "save_playlist_tracks");
  saved = args.tracks.map(t => ({...t}));
  return {id: 77, name: args.name, thumbnail: "", track_count: saved.length};
 });
 // Execute the actual component handler, so its count/message and persistence
 // path cannot drift from the tested bridge contract.
 const component = fs.readFileSync(new URL("../lib/components/QueueView.svelte", import.meta.url), "utf8");
 const start = component.indexOf("  async function confirmSave()");
 const end = component.indexOf("  function handleContext", start);
 assert.ok(start >= 0 && end > start);
 const messages = [];
 const context = {newPlaylistName: "My Queue", saving: false, savingAsPlaylist: true,
  queue: player.queue, savePlaylistTracks: bridge.savePlaylistTracks,
  toastState: {add: message => messages.push(message)}};
 const confirmSave = vm.runInNewContext(`(${component.slice(start, end).trim()})`, context);
 await confirmSave();
 assert.deepEqual(Array.from(saved, t => t.id), ["A", "A", "unavailable:missing", "B"]);
 assert.equal(saved[0].title, "A");
 assert.match(messages[0], /with 4 tracks/);
 assert.equal(context.savingAsPlaylist, false);
 assert.equal(context.saving, false);
});

test("removing the current occurrence loads its replacement through real PlayerState and IPC", async () => {
 for (const ids of [["A", "A", "B"], ["A", "B"], ["A", "unavailable:missing", "B"]]) {
  let bridge;
  const {player} = playerModule({playTrack: t => bridge.playTrack(t), stop: () => bridge.stop()});
  const calls = [];
  bridge = bridgeModule(player, async (name, args) => calls.push([name, args]));
  player.setQueue(ids.map(track));
  await bridge.playTrack(player.playFromQueue(0));
  calls.length = 0;
  player.removeFromQueue(0);
  await new Promise(resolve => setImmediate(resolve));
  const replacement = ids.slice(1).find(id => !id.startsWith("unavailable:"));
  assert.deepEqual(calls.filter(([name]) => name === "play_track").map(([, args]) => args.trackId), [replacement]);
  assert.equal(player.currentTrack.id, replacement);
  assert.equal(player.queueIndex, ids[1].startsWith("unavailable:") ? 1 : 0);
 }
});

test("browser session picker passes only the selected path to native import and cancellation does nothing", async () => {
 const {player} = playerModule();
 const status = {configured: true, connected: true, auth_method: "browser", identity_label: "UCfixture", oauth_connected: false};
 const calls = [];
 const selectedPath = "C:\\Private\\session.txt";
 const bridge = bridgeModule(player, async (name, args) => { calls.push([name, args]); return status; }, {}, {
  open: async options => { assert.equal(options.multiple, false); assert.equal(options.directory, false); return selectedPath; },
 });
 assert.equal(await bridge.ytmusicConnectBrowserFile(), status);
 assert.equal(calls.length, 1);
 assert.equal(calls[0][0], "ytmusic_import_browser_headers");
 assert.deepEqual(Object.keys(calls[0][1]), ["path"]);
 assert.equal(calls[0][1].path, selectedPath);
 const cancelled = bridgeModule(player, async () => assert.fail("cancelled picker must not invoke native import"), {}, {open: async () => null});
 assert.equal(await cancelled.ytmusicConnectBrowserFile(), null);
});

function accountHandler(name, next, context) {
 const source = fs.readFileSync(new URL("../lib/components/YouTubeAccount.svelte", import.meta.url), "utf8");
 const start = source.indexOf(` async function ${name}()`);
 const end = next ? source.indexOf(` async function ${next}()`, start) : source.indexOf("</script>", start);
 assert.ok(start >= 0 && end > start);
 return vm.runInNewContext(`(${source.slice(start, end).trim()})`, context);
}

test("account file connection preserves the current session on cancel/failure and suppresses results after navigation", async () => {
 const previous = {configured: true, connected: true, auth_method: "browser", identity_label: "UCprevious", oauth_connected: false};
 const base = () => ({status: previous, report: {complete: true}, busy: false, error: "", code: null, disposed: false, cancel: () => {}, refreshStatus: async () => {}});
 const cancelled = {...base(), ytmusicConnectBrowserFile: async () => null};
 await accountHandler("connectBrowser", "connect", cancelled)();
 assert.equal(cancelled.status, previous);
 assert.equal(cancelled.report.complete, true);
 assert.equal(cancelled.busy, false);
 const failed = {...base(), ytmusicConnectBrowserFile: async () => { throw new Error("Session expired; choose a new file"); }};
 await accountHandler("connectBrowser", "connect", failed)();
 assert.equal(failed.status, previous);
 assert.match(failed.error, /Session expired/);
 const verified = {configured: true, connected: true, auth_method: "browser", identity_label: "UCverified", oauth_connected: false};
 const success = {...base(), ytmusicConnectBrowserFile: async () => verified};
 await accountHandler("connectBrowser", "connect", success)();
 assert.equal(success.status, verified);
 assert.equal(success.report, null);
 assert.equal(success.busy, false);
 let resolve;
 const navigated = {...base(), ytmusicConnectBrowserFile: () => new Promise(done => resolve = done)};
 const pending = accountHandler("connectBrowser", "connect", navigated)();
 navigated.disposed = true;
 resolve(verified); await pending;
 assert.equal(navigated.status, previous);
});

test("OAuth completion refreshes native Music status instead of treating a Google grant as a working Music session", async () => {
 let completed; let refreshes = 0;
 const oauthOnly = {configured: true, connected: false, auth_method: "oauth", identity_label: null, oauth_connected: true};
 const context = {cancel: () => {}, error: "", code: null, busy: false, clientId: "fixture", clientSecret: "fixture", disposed: false, status: null,
  ytmusicConnect: async () => code, ytmusicPollLogin: async () => ({status: "connected", interval: 0}),
  pollDeviceLogin: (_code, _poll, done) => { completed = done; return () => {}; },
  refreshStatus: async () => { refreshes++; context.status = oauthOnly; },
 };
 await accountHandler("connect", "disconnect", context)();
 assert.equal(context.clientSecret, "");
 completed(); await new Promise(resolve => setImmediate(resolve));
 assert.equal(refreshes, 1);
 assert.equal(context.status.connected, false);
 assert.equal(context.busy, false);
});

test("expired library import refreshes native connection state while a rejected replacement retains a valid session", async () => {
 const {player} = playerModule();
 const valid = {configured: true, connected: true, auth_method: "browser", identity_label: "UCverified", oauth_connected: false};
 const expired = {...valid, connected: false};
 let nativeStatus = valid;
 const calls = [];
 const bridge = bridgeModule(player, async (name) => {
  calls.push(name);
  if (name === "ytmusic_status") return nativeStatus;
  if (name === "ytmusic_import_library") { nativeStatus = expired; throw new Error("YouTube Music session expired; reconnect"); }
  if (name === "ytmusic_import_browser_headers") throw new Error("Selected file is not a valid session");
  assert.fail(`Unexpected command ${name}`);
 }, {}, {open: async () => "C:\\Private\\invalid.txt"});
 const context = {status: valid, report: {complete: true}, busy: false, importing: false, error: "", code: null, disposed: false, cancel: () => {},
  ytmusicStatus: bridge.ytmusicStatus, ytmusicImportLibrary: bridge.ytmusicImportLibrary, ytmusicConnectBrowserFile: bridge.ytmusicConnectBrowserFile};
 context.refreshStatus = accountHandler("refreshStatus", "connectBrowser", context);
 await accountHandler("importAll", null, context)();
 assert.equal(context.status.connected, false);
 assert.equal(context.importing, false);
 assert.equal(context.report, null);
 assert.match(context.error, /session expired/);
 assert.deepEqual(calls, ["ytmusic_import_library", "ytmusic_status"]);
 // A bad replacement file does not invalidate the previously working native session.
 nativeStatus = valid; context.status = valid; calls.length = 0;
 await accountHandler("connectBrowser", "connect", context)();
 assert.equal(context.status, valid);
 assert.equal(context.busy, false);
 assert.match(context.error, /not a valid session/);
 assert.deepEqual(calls, ["ytmusic_import_browser_headers", "ytmusic_status"]);
});

test("a successful partial import refreshes expired native status and retains its report warnings", async () => {
 const partial = {complete: false, playlists: [{source_id: "PLfixture", title: "Fixture", local_id: 42,
  imported_count: 7, expected_count: 20, unavailable_count: 0, complete: false, warnings: ["YouTube Music session expired; previous verified snapshot retained"]}]};
 const expired = {configured: true, connected: false, auth_method: "browser", identity_label: "UCverified", oauth_connected: false};
 const calls = [];
 const bridge = bridgeModule(playerModule().player, async name => {
  calls.push(name);
  if (name === "ytmusic_import_library") return partial;
  if (name === "ytmusic_status") return expired;
  assert.fail(`Unexpected command ${name}`);
 });
 const context = {status: {...expired, connected: true}, report: null, error: "", importing: false, disposed: false,
  ytmusicStatus: bridge.ytmusicStatus, ytmusicImportLibrary: bridge.ytmusicImportLibrary};
 context.refreshStatus = accountHandler("refreshStatus", "connectBrowser", context);
 await accountHandler("importAll", null, context)();
 assert.equal(context.status.connected, false);
 assert.equal(context.report, partial);
 assert.equal(context.report.playlists[0].warnings[0], partial.playlists[0].warnings[0]);
 assert.equal(context.importing, false);
 assert.deepEqual(calls, ["ytmusic_import_library", "ytmusic_status"]);
});
