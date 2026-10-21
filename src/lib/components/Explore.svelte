<script lang="ts">
 import { onMount } from "svelte";
 import { getExplore, playTrack, ytmusicHome, ytmusicStatus, ytmusicBrowse } from "../ipc/bridge";
 import { player } from "../state/player.svelte";
 import { nav } from "../state/nav.svelte";
 import { isPlayable } from "../util/tracks";
 import ContextMenu from "./ContextMenu.svelte";
 import type { Track, YtmHome, YtmItem } from "../types";
 let home = $state<YtmHome | null>(null);
 let publicSections = $state<{title: string; tracks: Track[]}[]>([]);
 let connected = $state(false);
 let loading = $state(true);
 let error = $state("");
 let browse = $state<{title: string; tracks: Track[]} | null>(null);
 let ctxMenu: ReturnType<typeof ContextMenu>;
 let disposed = false;
 let request = 0;
 onMount(() => { load(); return () => { disposed = true; request++; }; });
 async function load() {
  loading = true; error = "";
  try {
   connected = (await ytmusicStatus()).connected;
   if (disposed) return;
   if (connected) {
    const result = await ytmusicHome();
    if (!result.personalized) throw new Error("YouTube Music did not return a personal home");
    if (!disposed) home = result;
   }
  } catch (e) { if (!disposed) error = String(e); }
  finally { if (!disposed) loading = false; }
 }
 async function loadPublic() {
  loading = true; error = "";
  try { const result = await getExplore(); if (!disposed) publicSections = result.sections; }
  catch (e) { if (!disposed) error = String(e); }
  finally { if (!disposed) loading = false; }
 }
 async function openItem(item: YtmItem) {
  if (item.kind === "track") {
   if (item.track) await play(item.track);
   else error = "YouTube Music returned a track without playback metadata";
   return;
  }
  const id = ++request;
  loading = true; error = "";
  try { const result = await ytmusicBrowse(item.id); if (!disposed && id === request) browse = result; }
  catch (e) { if (!disposed && id === request) error = String(e); }
  finally { if (!disposed && id === request) loading = false; }
 }
 async function play(track: Track) {
  try { await playTrack(track); } catch (e) { error = String(e); }
 }
 async function playBrowse(index: number) {
  if (!browse || !isPlayable(browse.tracks[index])) return;
  player.setQueue(browse.tracks);
  const track = player.playFromQueue(index);
  if (track) await play(track);
 }
</script>
<ContextMenu bind:this={ctxMenu} />
<div class="explore">
 <div class="section-header"><h2 class="section-title">{browse ? browse.title : "YouTube Music Explore"}</h2>
  {#if browse}<button onclick={() => { browse = null; request++; }}>Back to Explore</button>
  {:else if connected}<button disabled={loading} onclick={load}>Refresh</button>{/if}
 </div>
 {#if error}<p role="alert">{error}</p>{/if}
 {#if loading}<p role="status">Loading YouTube Music-</p>{/if}
 {#if !connected}
  <div class="section">
   <p>Connect your YouTube Music account to see your personal home and recommendations.</p>
   <button onclick={() => nav.activeTab = "account"}>Connect YouTube Music</button>
  </div>
 {/if}
 {#if browse}
  <div class="browse-list">
   {#each browse.tracks as track, i (i)}
    <button class:unavailable={!isPlayable(track)} disabled={!isPlayable(track)} onclick={() => playBrowse(i)} oncontextmenu={e => ctxMenu.open(e, track)}>
     <span>{i + 1}. {track.title}</span><span>{isPlayable(track) ? track.artist : "Unavailable - preserved in order"}</span>
    </button>
   {/each}
  </div>
 {:else if home}
  {#each home.sections as section, si (si)}
   <section class="section">
    <h2 class="section-title">{section.title}</h2>
    <div class="card-grid">
     {#each section.items as item, i (i)}
      <button class="card" onclick={() => openItem(item)} oncontextmenu={e => { if (item.track) ctxMenu.open(e, item.track); }}>
       <span class="art-shell">{#if item.thumbnail}<img class="card-thumb" src={item.thumbnail} alt="" loading="lazy" />{:else}<span class="art-fallback">{item.title.slice(0, 1)}</span>{/if}</span>
       <span class="card-info"><span class="card-title">{item.title}</span><span class="card-artist">{item.subtitle || item.kind}</span></span>
      </button>
     {/each}
    </div>
   </section>
  {/each}
 {/if}
 {#if !browse}
  <section class="section">
   <div class="section-header"><h2 class="section-title">Public discovery</h2><button disabled={loading} onclick={loadPublic}>Load public music</button></div>
   <p>Public discovery is separate from your YouTube Music personal home.</p>
   {#each publicSections as section, si (si)}
    <h3>{section.title}</h3><div class="card-grid">
     {#each section.tracks as track, i (i)}
      <button class="card" onclick={() => play(track)} oncontextmenu={e => ctxMenu.open(e, track)}>
       <span class="art-shell">{#if track.thumbnail}<img class="card-thumb" src={track.thumbnail} alt="" loading="lazy" />{/if}</span>
       <span class="card-info"><span class="card-title">{track.title}</span><span class="card-artist">{track.artist}</span></span>
      </button>
     {/each}
    </div>
   {/each}
  </section>
 {/if}
</div>
<style>
 .explore { display: flex; flex-direction: column; gap: 32px; animation: viewEnter 400ms var(--ease-out-expo); }
 .section { display: flex; flex-direction: column; gap: 14px; }
 .section-header { display: flex; align-items: center; justify-content: space-between; gap: 12px; }
 .section-title { font-size: 1.1rem; font-weight: 700; }
 .card-grid { display: flex; gap: 18px; overflow-x: auto; padding: 4px 0 14px; }
 .card { flex: 0 0 184px; display: flex; flex-direction: column; gap: 10px; padding: 10px; text-align: left; border: 1px solid transparent; border-radius: var(--radius); background: rgba(255,255,255,.02); color: var(--text-primary); }
 .card:hover, .card:focus-visible { background: var(--bg-elevated); border-color: var(--bg-overlay); }
 .art-shell { display: block; width: 100%; aspect-ratio: 1; overflow: hidden; border-radius: var(--radius); background: var(--bg-overlay); }
 .card-thumb { width: 100%; height: 100%; object-fit: cover; }
 .art-fallback { height: 100%; display: grid; place-items: center; color: var(--accent); font-size: 3rem; }
 .card-info { display: flex; flex-direction: column; gap: 4px; }
 .card-title { font-size: .92rem; font-weight: 650; overflow-wrap: anywhere; }
 .card-artist { font-size: .8rem; color: var(--text-secondary); }
 .browse-list { display: flex; flex-direction: column; gap: 8px; }
 .browse-list button { display: flex; justify-content: space-between; padding: 12px; background: var(--bg-elevated); color: var(--text-primary); border-radius: var(--radius-sm); text-align: left; gap: 16px; }
 .unavailable { opacity: .45; }
 .section-header > button, .section > button { background: var(--bg-elevated); padding: 10px 16px; color: var(--accent); border-radius: var(--radius-sm); }
 p { color: var(--text-secondary); }
 @media (max-width: 700px) { .card { flex-basis: 148px; } }
</style>
