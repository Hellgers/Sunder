import type { Track } from "../types";
export function isPlayable(track: Track | null | undefined): track is Track {
 return !!track && !track.id.startsWith("unavailable:");
}
export function playableIndex(tracks: Track[], from: number, direction: 1 | -1): number {
 for (let i = from; i >= 0 && i < tracks.length; i += direction) if (isPlayable(tracks[i])) return i;
 return -1;
}
