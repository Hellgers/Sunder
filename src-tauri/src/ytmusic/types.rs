use crate::models::Track;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct ConnectionStatus {
    pub configured: bool,
    pub connected: bool,
    pub auth_method: &'static str,
    pub identity_label: Option<String>,
    pub oauth_connected: bool,
}

#[derive(Serialize)]
pub struct LoginChallenge {
    pub user_code: String,
    pub verification_url: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Serialize)]
pub struct LoginProgress {
    pub status: String,
    pub interval: u64,
}

#[derive(Serialize)]
pub struct HomeItem {
    pub kind: String,
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub thumbnail: String,
    pub track: Option<Track>,
}

#[derive(Serialize)]
pub struct HomeSection {
    pub title: String,
    pub items: Vec<HomeItem>,
}

#[derive(Serialize)]
pub struct HomeResponse {
    pub sections: Vec<HomeSection>,
    pub personalized: bool,
}

#[derive(Serialize)]
pub struct BrowseResponse {
    pub title: String,
    pub tracks: Vec<Track>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub source_id: String,
    pub title: String,
    pub thumbnail: String,
    pub expected_count: Option<usize>,
}

/// Every source occurrence is retained, including duplicates and unavailable songs.
#[derive(Clone, Serialize, Deserialize)]
pub struct SourceTrack {
    pub position: usize,
    pub source_video_id: Option<String>,
    pub available: bool,
    pub track: Track,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SourcePlaylist {
    pub source: LibraryEntry,
    pub tracks: Vec<SourceTrack>,
    pub complete: bool,
    pub warnings: Vec<String>,
}

#[derive(Serialize)]
pub struct PlaylistImportReport {
    pub source_id: String,
    pub title: String,
    pub local_id: i64,
    pub expected_count: Option<usize>,
    pub imported_count: usize,
    pub unavailable_count: usize,
    pub complete: bool,
    pub warnings: Vec<String>,
}

#[derive(Serialize)]
pub struct ImportReport {
    pub playlists: Vec<PlaylistImportReport>,
    pub complete: bool,
}
