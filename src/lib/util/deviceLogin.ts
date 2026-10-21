import type { YtmDeviceCode } from "../types";
type PollResult = {status: "pending" | "connected" | "slow_down"; interval: number};
/** A single outstanding poll, cancellable on navigation; credentials never enter storage. */
export function pollDeviceLogin(code: YtmDeviceCode, poll: () => Promise<PollResult>, connected: () => void,
 error: (message: string) => void, clock = {now: () => Date.now(), set: setTimeout, clear: clearTimeout}): () => void {
 let cancelled = false;
 let interval = Math.max(1, code.interval) * 1000;
 const deadline = clock.now() + code.expires_in * 1000;
 let timer: ReturnType<typeof setTimeout>;
 const schedule = () => { timer = clock.set(run, Math.min(interval, Math.max(0, deadline - clock.now()))); };
 const run = async () => {
  if (cancelled) return;
  if (clock.now() >= deadline) { error("Sign-in code expired. Connect again."); return; }
  try {
   const result = await poll();
   if (cancelled) return;
   if (clock.now() >= deadline) { error("Sign-in code expired. Connect again."); return; }
   if (result.status === "connected") { connected(); return; }
   interval = Math.max(interval + (result.status === "slow_down" ? 5000 : 0), Math.max(1, result.interval) * 1000);
   schedule();
  } catch (e) { if (!cancelled) error(String(e)); }
 };
 schedule();
 return () => { cancelled = true; clock.clear(timer); };
}
