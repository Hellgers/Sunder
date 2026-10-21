use super::{types::*, YouTubeMusic};
use crate::{db::SearchCache, models::Track};
use tauri::State;

#[tauri::command]
pub async fn ytmusic_status(ytmusic: State<'_, YouTubeMusic>) -> Result<ConnectionStatus, String> {
    Ok(ytmusic.status().await)
}
#[tauri::command]
pub async fn ytmusic_import_browser_headers(
    path: String,
    ytmusic: State<'_, YouTubeMusic>,
) -> Result<ConnectionStatus, String> {
    ytmusic
        .import_browser_headers(std::path::Path::new(&path))
        .await
}
#[tauri::command]
pub async fn ytmusic_connect(
    client_id: String,
    client_secret: String,
    ytmusic: State<'_, YouTubeMusic>,
) -> Result<LoginChallenge, String> {
    ytmusic.connect(client_id, client_secret).await
}
#[tauri::command]
pub async fn ytmusic_poll_login(ytmusic: State<'_, YouTubeMusic>) -> Result<LoginProgress, String> {
    ytmusic.poll_login().await
}
#[tauri::command]
pub async fn ytmusic_disconnect(ytmusic: State<'_, YouTubeMusic>) -> Result<(), String> {
    ytmusic.disconnect().await
}
#[tauri::command]
pub async fn ytmusic_home(
    ytmusic: State<'_, YouTubeMusic>,
    db: State<'_, SearchCache>,
) -> Result<HomeResponse, String> {
    let result = ytmusic.home().await?;
    let tracks: Vec<Track> = result
        .sections
        .iter()
        .flat_map(|section| section.items.iter())
        .filter_map(|item| item.track.clone())
        .collect();
    db.upsert_tracks(&tracks)
        .map_err(|_| "Could not cache YouTube Music recommendations")?;
    Ok(result)
}
#[tauri::command]
pub async fn ytmusic_radio(
    video_id: String,
    ytmusic: State<'_, YouTubeMusic>,
    db: State<'_, SearchCache>,
) -> Result<Vec<Track>, String> {
    let result = ytmusic.radio(&video_id).await?;
    db.upsert_tracks(&result)
        .map_err(|_| "Could not cache YouTube Music radio")?;
    Ok(result)
}
#[tauri::command]
pub async fn ytmusic_import_library(
    ytmusic: State<'_, YouTubeMusic>,
    db: State<'_, SearchCache>,
) -> Result<ImportReport, String> {
    ytmusic.import_library(&db).await
}
#[tauri::command]
pub fn ytmusic_imported_playlist_ids(db: State<'_, SearchCache>) -> Result<Vec<i64>, String> {
    db.youtube_imported_playlist_ids()
        .map_err(|_| "Could not read imported playlists".into())
}
#[tauri::command]
pub async fn ytmusic_browse(
    browse_id: String,
    ytmusic: State<'_, YouTubeMusic>,
    db: State<'_, SearchCache>,
) -> Result<BrowseResponse, String> {
    let result = ytmusic.browse(&browse_id).await?;
    db.upsert_tracks(&result.tracks)
        .map_err(|_| "Could not cache YouTube Music songs")?;
    Ok(result)
}
