<script lang="ts">
 import { onMount } from "svelte";
 import { ytmusicStatus, ytmusicConnectBrowserFile, ytmusicConnect, ytmusicPollLogin, ytmusicDisconnect, ytmusicImportLibrary, openUrl } from "../ipc/bridge";
 import { pollDeviceLogin } from "../util/deviceLogin";
 import { nav } from "../state/nav.svelte";
 import type { YtmDeviceCode, YtmImportReport, YtmConnectionStatus } from "../types";
 let status = $state<YtmConnectionStatus | null>(null);
 let connected = $derived(status?.connected === true);
 let loading = $state(true);
 let clientId = $state("");
 let clientSecret = $state("");
 let code = $state<YtmDeviceCode | null>(null);
 let busy = $state(false);
 let importing = $state(false);
 let error = $state("");
 let report = $state<YtmImportReport | null>(null);
 let cancel = () => {};
 let disposed = false;
 onMount(() => {
  refreshStatus().finally(() => { if (!disposed) loading = false; });
  return () => { disposed = true; cancel(); clientSecret = ""; clientId = ""; };
 });
 async function refreshStatus() {
  try { const result = await ytmusicStatus(); if (!disposed) status = result; }
  catch (e) { if (!disposed) error = String(e); }
 }
 async function connectBrowser() {
  busy = true; error = ""; cancel(); code = null;
  try {
   const result = await ytmusicConnectBrowserFile();
   if (disposed || result === null) return;
   status = result;
   report = null;
   if (!result.connected) error = "YouTube Music could not verify this session. Follow the browser setup instructions and try again.";
  } catch (e) { if (!disposed) { error = String(e); await refreshStatus(); } }
  finally { if (!disposed) busy = false; }
 }
 async function connect() {
  cancel(); error = ""; code = null; busy = true;
  try {
   const result = await ytmusicConnect(clientId.trim(), clientSecret.trim());
   clientSecret = "";
   if (disposed) return;
   code = result;
   cancel = pollDeviceLogin(result, ytmusicPollLogin, () => {
    code = null;
    void refreshStatus().finally(() => { if (!disposed) busy = false; });
   }, message => { error = message; code = null; busy = false; });
  } catch (e) { if (!disposed) { error = String(e); busy = false; } }
 }
 async function disconnect() {
  busy = true; error = ""; cancel();
  try { await ytmusicDisconnect(); if (!disposed) { status = null; code = null; report = null; } }
  catch (e) { error = String(e); }
  finally { busy = false; }
 }
 async function importAll() {
  importing = true; error = ""; report = null;
  try { const result = await ytmusicImportLibrary(); if (!disposed) report = result; }
  catch (e) { if (!disposed) error = String(e); }
  finally {
   if (!disposed) {
    await refreshStatus();
    if (!disposed) importing = false;
   }
  }
 }
</script>
<section class="account">
 <h2>YouTube Music account</h2>
 <p>Connect your signed-in YouTube Music browser session for your personal home, track radio, and library import.</p>
 {#if error}<p role="alert" class="error">{error}</p>{/if}
 {#if loading}
  <p role="status">Checking connection...</p>
 {:else if connected}
  <p role="status">Connected to YouTube Music through your browser session</p>
  {#if status?.identity_label}<p class="identity">Channel: {status.identity_label}</p>{/if}
  <div class="actions">
   <button onclick={() => { nav.activeTab = "explore"; }}>Open personal Explore</button>
   <button onclick={importAll} disabled={importing || busy}>{importing ? "Importing all playlists and Liked Music..." : "Import all playlists + Liked Music"}</button>
   <button onclick={disconnect} disabled={busy || importing}>Disconnect</button>
   <button onclick={connectBrowser} disabled={busy || importing}>{busy ? "Verifying session..." : "Replace browser session from file"}</button>
  </div>
  {#if importing}<p role="status">Fetching every page. Large libraries can take several minutes. Keep this page open.</p>{/if}
 {:else}
  <div class="browser-connect">
   <h3>Connect a browser session from a file</h3>
   <ol>
    <li>Open <button class="text-link" onclick={() => openUrl("https://music.youtube.com").catch(e => error = String(e))}>YouTube Music</button> in your browser and sign in to the account you want to use.</li>
    <li>Press Ctrl+Shift+I, open Network, filter by <code>/browse</code>, then open Library. Select a successful POST request to music.youtube.com.</li>
    <li>Firefox: copy request headers. Chrome or Edge: choose Copy → Copy as fetch (Node.js). Save the copied text in a private UTF-8 <code>.txt</code> file.</li>
   </ol>
   <button class="connect-primary" onclick={connectBrowser} disabled={busy}>{busy ? "Verifying session..." : "Choose session file and connect"}</button>
   <p>The native app reads the file and verifies your account. Keep the file private; you can delete it after a successful connection.</p>
  </div>
  {#if status?.oauth_connected}
   <p role="status">OAuth credentials are saved, but YouTube Music currently rejects TV OAuth access. Connect a browser session above to use your account.</p>
   <button onclick={disconnect} disabled={busy}>Remove saved credentials</button>
  {/if}
  <details>
   <summary>Advanced setup and OAuth</summary>
   <p>Session files can contain raw request headers, a JSON header object, or Copy as fetch (Node.js) text.</p>
   <button onclick={() => openUrl("https://ytmusicapi.readthedocs.io/en/stable/setup/browser.html").catch(e => error = String(e))}>Open browser setup guide</button>
   <h3>TV OAuth client</h3>
   <p>YouTube Music currently rejects this sign-in method for this account. A successful Google sign-in alone does not connect Music features.</p>
   <label>OAuth client ID<input bind:value={clientId} autocomplete="off" spellcheck="false" disabled={busy} /></label>
   <label>OAuth client secret<input type="password" bind:value={clientSecret} autocomplete="off" disabled={busy} /></label>
   <button onclick={connect} disabled={busy || !clientId.trim() || !clientSecret.trim()}>Authorize OAuth client</button>
   {#if code}
    <p role="status">Enter code <strong>{code.user_code}</strong> at Google's verification page. Waiting for sign-in...</p>
    <button onclick={() => code && openUrl(code.verification_url).catch(e => error = String(e))}>Open Google verification</button>
    <p>{code.verification_url}</p>
   {/if}
  </details>
 {/if}
 {#if report}
  <p role="status">{report.complete ? "Import complete" : "Import incomplete - review details below"} - {report.playlists.length} playlists</p>
  {#each report.playlists as item, i (i)}
   <article>
    <button disabled={item.local_id <= 0} onclick={() => nav.jumpToPlaylist(item.local_id, item.title)}>{item.title}</button>
    <p>{item.imported_count} / {item.expected_count ?? "unknown"} entries - {item.unavailable_count} unavailable - {item.complete ? "complete" : "incomplete"}</p>
    {#each item.warnings as warning}<p class="error">{warning}</p>{/each}
   </article>
  {/each}
 {/if}
</section>
<style>
 .account { max-width: 850px; display: flex; flex-direction: column; gap: 18px; }
 p { color: var(--text-secondary); line-height: 1.5; }
 label { display: grid; gap: 8px; }
 input { background: var(--bg-elevated); color: var(--text-primary); padding: 12px; border: 1px solid var(--bg-overlay); border-radius: var(--radius-sm); }
 button { background: var(--bg-elevated); color: var(--text-primary); padding: 10px 16px; border-radius: var(--radius-sm); text-align: left; }
 button:hover { color: var(--accent); } button:disabled { opacity: .5; }
 .actions { display: flex; gap: 10px; flex-wrap: wrap; } .error { color: var(--error, #ef7777); }
 article { border-top: 1px solid var(--bg-overlay); padding-top: 10px; }
 .browser-connect, details { display: flex; flex-direction: column; gap: 14px; padding: 18px; border: 1px solid var(--bg-overlay); border-radius: var(--radius-sm); }
 details > :not(summary) { margin-top: 14px; }
 summary { cursor: pointer; color: var(--text-secondary); }
 ol { padding-left: 22px; display: grid; gap: 12px; color: var(--text-secondary); line-height: 1.5; }
 code { color: var(--text-primary); }
 .text-link { display: inline; padding: 0; background: transparent; color: var(--accent); text-decoration: underline; }
 .connect-primary { color: var(--accent); }
 .identity { overflow-wrap: anywhere; }
 strong { color: var(--accent); font-size: 1.3em; }
</style>
