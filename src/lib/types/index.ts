export interface Track {
  id: string;
  title: string;
  artist: string;
  thumbnail: string;
  duration_secs: number;
  stream_url?: string;
}

export interface SearchResult {
  tracks: Track[];
  source: "local" | "remote";
}

export interface PlaybackProgress {
  position_ms: number;
  duration_ms: number;
  state: string;
  volume: number;
  speed: number;
}

export interface Playlist {
  id: number;
  name: string;
  thumbnail: string;
  track_count: number;
}

export interface ExploreSection {
  title: string;
  tracks: Track[];
}

export interface ExploreData {
  sections: ExploreSection[];
}

export interface EqSettings {
  enabled: boolean;
  gains: number[];
}

export type DownloadStatus = "queued" | "downloading" | "converting" | "done" | "error";

export interface DownloadEvent {
  track_id: string;
  status: DownloadStatus;
  percent: number;
}

export interface PlaybackQuality {
 video_id: string; source: "stream" | "offline"; codec: string | null;
 bitrate_kbps: number | null; sample_rate_hz: number | null; channels: number | null; format_id: string | null;
}
export interface YtmItem {
 kind: "track" | "playlist" | "album" | "artist";
 id: string; title: string; subtitle: string; thumbnail: string; track?: Track;
}
export interface YtmHome { sections: {title: string; items: YtmItem[]}[]; personalized: boolean }
export interface YtmDeviceCode { user_code: string; verification_url: string; expires_in: number; interval: number }
export interface YtmConnectionStatus {
 configured: boolean;
 /** True after the native app verifies the browser session with YouTube Music. */
 connected: boolean;
 auth_method: "browser" | "oauth" | "none";
 identity_label: string | null;
 oauth_connected: boolean;
}
export interface YtmImportReport {
 playlists: {source_id: string; title: string; local_id: number; expected_count: number | null;
 imported_count: number; unavailable_count: number; complete: boolean; warnings: string[]}[];
 complete: boolean;
}
