//! Read-only YouTube Music adapter. The Music endpoints are unofficial and may change.
//! User-selected browser headers authenticate Music; OAuth is kept for the official Data API.
mod auth;
pub mod commands;
mod parse;
pub mod types;

use crate::db::SearchCache;
use crate::models::Track;
use reqwest::{header::HeaderMap, Client};
use serde_json::{json, Value};
use std::{collections::HashSet, path::Path, time::Duration};
use tokio::sync::Mutex;
use types::*;

pub struct YouTubeMusic {
    client: Client,
    auth: Mutex<auth::Auth>,
    import_lock: Mutex<()>,
}

impl YouTubeMusic {
    pub fn new(_data_dir: &Path) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(40))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("Could not initialize YouTube Music HTTP client"),
            auth: Mutex::new(auth::Auth::new(auth::credential_path())),
            import_lock: Mutex::new(()),
        }
    }

    pub async fn status(&self) -> ConnectionStatus {
        self.auth.lock().await.status()
    }

    pub async fn import_browser_headers(&self, path: &Path) -> Result<ConnectionStatus, String> {
        let _guard = self
            .import_lock
            .try_lock()
            .map_err(|_| "Wait for the library import to finish before changing accounts")?;
        let mut browser = auth::BrowserCredentials::from_file(path)?;
        // Verify the browser's own account. Never borrow an identity from a separate OAuth login.
        browser.set_verified_channel(self.browser_channel(browser.headers(auth::now())?).await?)?;
        let mut auth = self.auth.lock().await;
        auth.save_browser(browser)?;
        Ok(auth.status())
    }

    async fn browser_channel(&self, headers: HeaderMap) -> Result<String, String> {
        let menu = send_music(
            &self.client,
            "account/account_menu",
            json!({}),
            None,
            Some(headers.clone()),
        )
        .await?;
        let profile = account_profile(&menu)?;
        if let Some(channel) = profile.channel_id {
            return Ok(channel);
        }
        let handle = profile.handle.ok_or("Could not verify the browser channel; open your own Music profile and export its request headers again")?;
        let mut url =
            reqwest::Url::parse(auth::MUSIC_ORIGIN).map_err(|_| "Invalid Music origin")?;
        url.set_path(&format!("/{handle}"));
        let resolved = send_music(
            &self.client,
            "navigation/resolve_url",
            json!({"url":url.as_str()}),
            None,
            Some(headers),
        )
        .await?;
        resolved.pointer("/endpoint/browseEndpoint/browseId").and_then(Value::as_str)
            .filter(|id| auth::valid_channel_id(id)).map(str::to_owned)
            .ok_or_else(|| "Could not resolve the authenticated Music profile to a stable channel ID; no session was changed".into())
    }

    async fn verified_browser_namespace(&self) -> Result<String, String> {
        let ((generation, headers), expected) = {
            let auth = self.auth.lock().await;
            (
                auth.browser_headers()?
                    .ok_or("Connect a browser session before importing your library")?,
                auth.browser_namespace()?,
            )
        };
        let channel = self.browser_channel(headers).await;
        if channel
            .as_ref()
            .is_err_and(|error| is_browser_rejection(error))
        {
            self.auth.lock().await.invalidate_browser(generation)?;
        }
        let actual = format!("youtube-channel:{}", channel?);
        if actual != expected {
            return Err("The browser channel changed; reconnect it before importing, so accounts cannot be mixed".into());
        }
        Ok(actual)
    }
    pub async fn connect(
        &self,
        client_id: String,
        client_secret: String,
    ) -> Result<LoginChallenge, String> {
        let _guard = self
            .import_lock
            .try_lock()
            .map_err(|_| "Wait for the library import to finish before changing accounts")?;
        self.auth
            .lock()
            .await
            .begin(&self.client, client_id, client_secret)
            .await
    }
    pub async fn poll_login(&self) -> Result<LoginProgress, String> {
        self.auth.lock().await.poll(&self.client).await
    }
    pub async fn disconnect(&self) -> Result<(), String> {
        let _guard = self
            .import_lock
            .try_lock()
            .map_err(|_| "Wait for the library import to finish before disconnecting")?;
        self.auth.lock().await.disconnect()
    }

    async fn request(
        &self,
        endpoint: &str,
        body: Value,
        continuation: Option<&str>,
    ) -> Result<Value, String> {
        let session = self.auth.lock().await.browser_headers()?;
        if session.is_none() && endpoint != "next" {
            return Err("Connect a browser session for personalized YouTube Music; Google OAuth alone cannot access Music".into());
        }
        let (generation, headers) = session.map_or((None, None), |(generation, headers)| {
            (Some(generation), Some(headers))
        });
        let result = send_music(&self.client, endpoint, body, continuation, headers).await;
        if result
            .as_ref()
            .is_err_and(|error| is_browser_rejection(error))
        {
            if let Some(generation) = generation {
                self.auth.lock().await.invalidate_browser(generation)?;
            }
        }
        result
    }

    pub async fn home(&self) -> Result<HomeResponse, String> {
        let response = self
            .request("browse", json!({"browseId":"FEmusic_home"}), None)
            .await?;
        let mut sections = parse::home_sections(&response);
        let mut next = parse::find(&response, "sectionListRenderer").and_then(parse::continuation);
        let mut seen = HashSet::new();
        // Home is an endless recommendation feed, unlike library import. Load useful rows.
        while sections.len() < 12 {
            let Some(token) = next.take() else {
                break;
            };
            if !seen.insert(token.clone()) {
                break;
            }
            let response = self
                .request("browse", json!({"browseId":"FEmusic_home"}), Some(&token))
                .await?;
            sections.extend(parse::home_sections(&response));
            next = parse::find(&response, "sectionListContinuation").and_then(parse::continuation);
        }
        if sections.is_empty() {
            return Err(
                "YouTube Music returned no supported personalized recommendation rows".into(),
            );
        }
        Ok(HomeResponse {
            sections,
            personalized: true,
        })
    }

    pub async fn radio(&self, video_id: &str) -> Result<Vec<Track>, String> {
        if video_id.len() != 11
            || !video_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err("Radio requires a valid YouTube song ID".into());
        }
        let response = self.request("next", json!({"videoId":video_id,"playlistId":format!("RDAMVM{video_id}"),"params":"wAEB","isAudioOnly":true,"enablePersistentPlaylistPanel":true,"tunerSettingValue":"AUTOMIX_SETTING_NORMAL"}), None).await?;
        let tracks = parse::radio_tracks(&response);
        if tracks.is_empty() {
            return Err("YouTube Music returned an empty radio queue".into());
        }
        Ok(tracks)
    }

    pub async fn browse(&self, browse_id: &str) -> Result<BrowseResponse, String> {
        if browse_id.is_empty()
            || browse_id.len() > 256
            || !browse_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err("Invalid YouTube Music browse ID".into());
        }
        if browse_id.starts_with("PL")
            || browse_id.starts_with("OLAK")
            || browse_id.starts_with("RD")
            || browse_id == "LM"
            || browse_id == "SE"
            || browse_id.starts_with("VL")
        {
            let playlist = self
                .playlist(LibraryEntry {
                    source_id: browse_id.strip_prefix("VL").unwrap_or(browse_id).into(),
                    title: String::new(),
                    thumbnail: String::new(),
                    expected_count: None,
                })
                .await?;
            return Ok(BrowseResponse {
                title: playlist.source.title,
                tracks: playlist
                    .tracks
                    .into_iter()
                    .filter(|row| row.available)
                    .map(|row| row.track)
                    .collect(),
            });
        }
        let response = self
            .request("browse", json!({"browseId":browse_id}), None)
            .await?;
        let title = parse::playlist_title(&response)
            .or_else(|| {
                parse::find(&response, "musicImmersiveHeaderRenderer")
                    .and_then(|r| r.get("title"))
                    .map(parse::text)
            })
            .unwrap_or_else(|| "YouTube Music".into());
        let tracks = parse::browse_tracks(&response);
        if tracks.is_empty() {
            return Err("YouTube Music returned no playable songs for this item".into());
        }
        Ok(BrowseResponse { title, tracks })
    }

    async fn library(&self) -> Result<Vec<LibraryEntry>, String> {
        let mut entries = vec![LibraryEntry {
            source_id: "LM".into(),
            title: "Liked Music".into(),
            thumbnail: String::new(),
            expected_count: None,
        }];
        let mut next = None;
        let mut seen_tokens = HashSet::new();
        let mut seen_ids = HashSet::from(["LM".to_owned()]);
        loop {
            let response = self
                .request(
                    "browse",
                    json!({"browseId":"FEmusic_liked_playlists"}),
                    next.as_deref(),
                )
                .await?;
            let container = parse::library_container(&response)
                .ok_or("YouTube Music library layout changed; no playlists were imported")?;
            if parse::unknown_library_rows(container) > 0 {
                return Err("YouTube Music returned unsupported library rows; the full playlist inventory could not be verified".into());
            }
            entries.extend(
                parse::library_entries(container)
                    .into_iter()
                    .filter(|entry| seen_ids.insert(entry.source_id.clone())),
            );
            next = parse::continuation(container);
            match &next {
                None => break,
                Some(token) if !seen_tokens.insert(token.clone()) => return Err("YouTube Music repeated a library continuation; the full library could not be verified".into()),
                _ => (),
            }
        }
        Ok(entries)
    }

    pub async fn playlist(&self, entry: LibraryEntry) -> Result<SourcePlaylist, String> {
        let browse_id = format!("VL{}", entry.source_id);
        let mut collector = parse::PlaylistCollector::new(entry);
        let mut next = None;
        loop {
            let result = self
                .request("browse", json!({"browseId":browse_id}), next.as_deref())
                .await;
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    collector.failure(error)?;
                    break;
                }
            };
            match collector.consume(&response) {
                Ok(token) => next = token,
                Err(error) => {
                    collector.failure(error)?;
                    break;
                }
            }
            if next.is_none() {
                break;
            }
        }
        Ok(collector.finish())
    }

    pub async fn import_library(&self, db: &SearchCache) -> Result<ImportReport, String> {
        let _guard = self
            .import_lock
            .try_lock()
            .map_err(|_| "A YouTube Music library import is already running")?;
        let namespace = self.verified_browser_namespace().await?;
        let entries = self.library().await?;
        let mut playlists = vec![];
        for entry in entries {
            let source_id = entry.source_id.clone();
            let title = entry.title.clone();
            match self.playlist(entry).await {
                Ok(playlist) => {
                    let (local_id, saved) = db
                        .import_youtube_snapshot(&namespace, &playlist)
                        .map_err(|_| "Could not save imported playlist")?;
                    let mut warnings = playlist.warnings;
                    if !saved {
                        warnings.push("The previous verified snapshot was retained because the new response was incomplete".into());
                    }
                    playlists.push(PlaylistImportReport {
                        source_id: playlist.source.source_id,
                        title: playlist.source.title,
                        local_id,
                        expected_count: playlist.source.expected_count,
                        imported_count: if saved { playlist.tracks.len() } else { 0 },
                        unavailable_count: if saved {
                            playlist.tracks.iter().filter(|t| !t.available).count()
                        } else {
                            0
                        },
                        complete: playlist.complete,
                        warnings,
                    });
                }
                Err(error) => playlists.push(PlaylistImportReport {
                    source_id,
                    title,
                    local_id: 0,
                    expected_count: None,
                    imported_count: 0,
                    unavailable_count: 0,
                    complete: false,
                    warnings: vec![error],
                }),
            }
        }
        let complete = playlists.iter().all(|p| p.complete);
        Ok(ImportReport {
            playlists,
            complete,
        })
    }
}

// All destinations and operations are fixed here. Imported URLs, Authorization headers,
// bodies and arbitrary headers are never forwarded or executed.
async fn send_music(
    client: &Client,
    endpoint: &str,
    mut body: Value,
    continuation: Option<&str>,
    headers: Option<HeaderMap>,
) -> Result<Value, String> {
    if !matches!(
        endpoint,
        "browse" | "next" | "account/account_menu" | "navigation/resolve_url"
    ) {
        return Err("YouTube Music adapter permits read-only endpoints only".into());
    }
    let authenticated = headers.is_some();
    let version = format!("1.{}.01.00", chrono::Utc::now().format("%Y%m%d"));
    body["context"] =
        json!({"client":{"clientName":"WEB_REMIX","clientVersion":version,"hl":"en"},"user":{}});
    if let Some(token) = continuation {
        body["continuation"] = json!(token);
    }
    let mut request = client
        .post(format!("{}/youtubei/v1/{endpoint}", auth::MUSIC_ORIGIN))
        .query(&[("alt", "json")])
        .header("Content-Type", "application/json")
        .header("Origin", auth::MUSIC_ORIGIN)
        .header("User-Agent", auth::MUSIC_USER_AGENT);
    if let Some(headers) = headers {
        // Public web-client API identifier used by upstream ytmusicapi for browser auth.
        request = request
            .query(&[("key", "AIzaSyC9XL3ZjWddXya6X74dJoCTL-WEYFDNX30")])
            .headers(headers);
    } else {
        request = request.header("Cookie", "SOCS=CAI");
    }
    if let Some(token) = continuation {
        request = request.query(&[("ctoken", token), ("continuation", token), ("type", "next")]);
    }
    let response = request
        .body(body.to_string())
        .send()
        .await
        .map_err(|_| "Could not reach YouTube Music")?;
    match response.status().as_u16() {
        401 | 403 => return Err(auth::BROWSER_EXPIRED_ERROR.into()),
        429 => return Err("YouTube Music request limit reached; try again later".into()),
        200..=299 => (),
        _ => return Err("YouTube Music could not complete this request".into()),
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|_| "Could not read YouTube Music response")?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| "YouTube Music returned an invalid response")?;
    if value.get("error").is_some() {
        return Err("YouTube Music returned an API error".into());
    }
    if value
        .pointer("/responseContext/mainAppWebResponseContext/loggedOut")
        .and_then(Value::as_bool)
        == Some(true)
        && authenticated
    {
        return Err(auth::BROWSER_SIGNED_OUT_ERROR.into());
    }
    Ok(value)
}

fn is_browser_rejection(error: &str) -> bool {
    matches!(
        error,
        auth::BROWSER_EXPIRED_ERROR | auth::BROWSER_SIGNED_OUT_ERROR
    )
}

struct AccountProfile {
    channel_id: Option<String>,
    handle: Option<String>,
}

fn account_profile(response: &Value) -> Result<AccountProfile, String> {
    let menu = response.get("actions").and_then(Value::as_array)
        .and_then(|actions| actions.iter().find_map(|action| action.pointer("/openPopupAction/popup/multiPageMenuRenderer")))
        .ok_or("Could not verify a signed-in Music account; export headers from an authenticated Music request")?;
    let header = menu
        .pointer("/header/activeAccountHeaderRenderer")
        .ok_or("The browser session has no active Music account")?;
    let mut channels = HashSet::new();
    if let Some(id) = header
        .pointer("/navigationEndpoint/browseEndpoint/browseId")
        .and_then(Value::as_str)
        .filter(|id| auth::valid_channel_id(id))
    {
        channels.insert(id.to_owned());
    }
    fn own_links(value: &Value, channels: &mut HashSet<String>) {
        match value {
            Value::Object(object) => {
                for key in ["compactLinkRenderer", "menuNavigationItemRenderer"] {
                    if let Some(link) = object.get(key) {
                        if matches!(
                            link.pointer("/icon/iconType").and_then(Value::as_str),
                            Some("ACCOUNT_BOX" | "ACCOUNT_CIRCLE")
                        ) {
                            if let Some(id) = link
                                .pointer("/navigationEndpoint/browseEndpoint/browseId")
                                .and_then(Value::as_str)
                                .filter(|id| auth::valid_channel_id(id))
                            {
                                channels.insert(id.to_owned());
                            }
                        }
                    }
                }
                for child in object.values() {
                    own_links(child, channels);
                }
            }
            Value::Array(values) => {
                for child in values {
                    own_links(child, channels);
                }
            }
            _ => (),
        }
    }
    if let Some(sections) = menu.get("sections") {
        own_links(sections, &mut channels);
    }
    if channels.len() > 1 {
        return Err(
            "The Music account menu has ambiguous channel identities; no session was changed"
                .into(),
        );
    }
    let handle = header
        .get("channelHandle")
        .map(parse::text)
        .filter(|handle| {
            handle.starts_with('@')
                && handle.len() <= 200
                && handle.len() > 1
                && !handle
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '/' | '\\' | '?' | '#'))
        });
    Ok(AccountProfile {
        channel_id: channels.into_iter().next(),
        handle,
    })
}

#[cfg(test)]
mod browser_tests {
    use super::*;

    fn menu(sections: Value) -> Value {
        json!({"actions":[{"openPopupAction":{"popup":{"multiPageMenuRenderer":{
            "header":{"activeAccountHeaderRenderer":{"accountName":{"runs":[{"text":"Fixture"}]},"channelHandle":{"runs":[{"text":"@fixture"}]}}},
            "sections":sections
        }}}}]})
    }

    #[test]
    fn browser_identity_only_uses_the_authenticated_own_profile() {
        let account = menu(json!([{"multiPageMenuSectionRenderer":{"items":[
            {"compactLinkRenderer":{"icon":{"iconType":"ACCOUNT_BOX"},"navigationEndpoint":{"browseEndpoint":{"browseId":"UCaaaaaaaaaaaaaaaaaaaaaa"}}}},
            {"compactLinkRenderer":{"icon":{"iconType":"OTHER"},"navigationEndpoint":{"browseEndpoint":{"browseId":"UCbbbbbbbbbbbbbbbbbbbbbb"}}}}
        ]}}]));
        let profile = account_profile(&account).unwrap();
        assert_eq!(
            profile.channel_id.as_deref(),
            Some("UCaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(profile.handle.as_deref(), Some("@fixture"));
        assert!(account_profile(&json!({"browseId":"UCbbbbbbbbbbbbbbbbbbbbbb"})).is_err());
        let profile = account_profile(&menu(json!([]))).unwrap();
        assert!(profile.channel_id.is_none());
        assert_eq!(profile.handle.as_deref(), Some("@fixture"));
    }

    #[test]
    fn browser_account_menu_ambiguity_never_picks_a_random_channel() {
        let account = menu(
            json!([{"compactLinkRenderer":{"icon":{"iconType":"ACCOUNT_BOX"},"navigationEndpoint":{"browseEndpoint":{"browseId":"UCaaaaaaaaaaaaaaaaaaaaaa"}}}},
            {"compactLinkRenderer":{"icon":{"iconType":"ACCOUNT_BOX"},"navigationEndpoint":{"browseEndpoint":{"browseId":"UCbbbbbbbbbbbbbbbbbbbbbb"}}}}]),
        );
        assert!(account_profile(&account).is_err());
    }

    #[tokio::test]
    async fn music_request_whitelist_refuses_mutation_before_any_network_request() {
        for endpoint in [
            "playlist/create",
            "like/like",
            "https://evil.example/",
            "browse/../../other",
        ] {
            assert!(send_music(&Client::new(), endpoint, json!({}), None, None)
                .await
                .unwrap_err()
                .contains("read-only"));
        }
    }

    #[tokio::test]
    #[ignore = "read-only live network check; never reads stored credentials"]
    async fn real_public_radio_uses_anonymous_music_and_returns_playable_tracks() {
        let music = YouTubeMusic {
            client: Client::builder()
                .timeout(Duration::from_secs(40))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            auth: Mutex::new(auth::Auth::anonymous()),
            import_lock: Mutex::new(()),
        };
        assert!(!music.status().await.connected);
        let tracks = music
            .radio("dQw4w9WgXcQ")
            .await
            .expect("public radio should resolve without credentials");
        let playable = tracks.iter().filter(|track| track.id.len() == 11).count();
        println!(
            "PUBLIC_RADIO track_count={} playable_count={playable}",
            tracks.len()
        );
        assert!(playable > 0);
    }
}
