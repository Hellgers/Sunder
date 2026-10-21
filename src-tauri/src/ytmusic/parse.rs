use super::types::*;
use crate::models::Track;
use serde_json::Value;
use std::collections::HashSet;

pub struct PlaylistCollector {
    entry: LibraryEntry,
    tracks: Vec<SourceTrack>,
    warnings: Vec<String>,
    complete: bool,
    seen: HashSet<String>,
    pages: usize,
}

impl PlaylistCollector {
    pub fn new(entry: LibraryEntry) -> Self {
        Self {
            entry,
            tracks: vec![],
            warnings: vec![],
            complete: true,
            seen: HashSet::new(),
            pages: 0,
        }
    }

    pub fn consume(&mut self, response: &Value) -> Result<Option<String>, String> {
        if self.pages == 0 {
            if let Some(title) = playlist_title(response) {
                self.entry.title = title;
            }
            self.entry.expected_count = expected_count(response).or(self.entry.expected_count);
        }
        self.pages += 1;
        let Some(container) = playlist_container(response) else {
            if self.entry.expected_count == Some(0) {
                return Ok(None);
            }
            return Err(
                "YouTube Music playlist layout changed; refusing to claim a full import".into(),
            );
        };
        // A verified zero-episode playlist uses an explicit empty-state message
        // and omits contents entirely. Do not treat arbitrary unknown rows as empty.
        if self.entry.source_id == "SE"
            && self.entry.expected_count == Some(0)
            && container.get("contents").is_none()
            && container
                .get("emptyState")
                .and_then(Value::as_object)
                .is_some_and(|empty| {
                    empty.len() == 1 && empty.get("messageRenderer").is_some_and(Value::is_object)
                })
            && container.as_object().is_some_and(|object| {
                object.keys().all(|key| {
                    matches!(
                        key.as_str(),
                        "collapsedItemCount"
                            | "contentsMultiSelectable"
                            | "contentsReorderable"
                            | "emptyState"
                            | "header"
                            | "playlistId"
                            | "targetId"
                            | "trackingParams"
                    )
                })
            })
            && continuation(container).is_none()
        {
            return Ok(None);
        }
        let (rows, unknown) = playlist_rows(container, &self.entry.source_id, self.tracks.len());
        if unknown > 0 {
            self.complete = false;
            self.warnings
                .push(format!("{unknown} source rows used an unsupported format"));
        }
        self.tracks.extend(rows);
        let next = continuation(container);
        if let Some(token) = &next {
            if !self.seen.insert(token.clone()) {
                self.complete = false;
                self.warnings.push(
                    "Source repeated a continuation; remaining songs could not be verified".into(),
                );
                return Ok(None);
            }
        }
        Ok(next)
    }

    pub fn failure(&mut self, error: String) -> Result<(), String> {
        if self.tracks.is_empty() {
            return Err(error);
        }
        self.complete = false;
        self.warnings.push(error);
        Ok(())
    }

    pub fn finish(mut self) -> SourcePlaylist {
        if let Some(expected) = self.entry.expected_count {
            if expected != self.tracks.len() {
                self.complete = false;
                self.warnings.push(format!(
                    "Source reports {expected} songs, received {} occurrences",
                    self.tracks.len()
                ));
            }
        } else {
            self.complete = false;
            self.warnings.push("All returned pages were read; source did not supply an exact total for count verification".into());
        }
        SourcePlaylist {
            source: self.entry,
            tracks: self.tracks,
            complete: self.complete,
            warnings: self.warnings,
        }
    }
}

pub fn find<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|v| find(v, key))),
        Value::Array(items) => items.iter().find_map(|v| find(v, key)),
        _ => None,
    }
}

pub fn text(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        return s.to_owned();
    }
    if let Some(s) = value.get("simpleText").and_then(Value::as_str) {
        return s.to_owned();
    }
    value
        .get("runs")
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .filter_map(|v| v.get("text").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default()
}

fn thumbnail(value: &Value) -> String {
    find(value, "thumbnails")
        .and_then(Value::as_array)
        .and_then(|v| v.last())
        .and_then(|v| v.get("url"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn flex(value: &Value, index: usize) -> String {
    value
        .get("flexColumns")
        .and_then(Value::as_array)
        .and_then(|v| v.get(index))
        .and_then(|v| v.get("musicResponsiveListItemFlexColumnRenderer"))
        .and_then(|v| v.get("text"))
        .map(text)
        .unwrap_or_default()
}

fn title_endpoint(renderer: &Value) -> Option<&Value> {
    renderer.pointer("/title/runs/0/navigationEndpoint")
}

fn playback_endpoint(renderer: &Value) -> Option<&Value> {
    renderer.pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint")
        .or_else(|| renderer.pointer("/navigationEndpoint/watchEndpoint"))
        .or_else(|| title_endpoint(renderer).and_then(|endpoint| endpoint.get("watchEndpoint")))
        .or_else(|| renderer.pointer("/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/navigationEndpoint/watchEndpoint"))
}

fn library_entry(item: &Value) -> Result<Option<LibraryEntry>, ()> {
    if [
        "continuationItemRenderer",
        "musicCreatePlaylistButtonRenderer",
    ]
    .iter()
    .any(|key| item.get(*key).is_some())
    {
        return Ok(None);
    }
    let r = item
        .get("musicTwoRowItemRenderer")
        .or_else(|| item.get("musicResponsiveListItemRenderer"))
        .ok_or(())?;
    // A recognized renderer without a resolvable destination is not evidence that
    // the source list is empty. Only an explicit create-playlist action is skipped.
    if r.pointer("/navigationEndpoint/modalEndpoint").is_some()
        || [
            "musicCreatePlaylistFormRenderer",
            "createPlaylistServiceEndpoint",
            "createPlaylistEndpoint",
        ]
        .iter()
        .any(|key| find(r, key).is_some())
    {
        return Ok(None);
    }
    let endpoint = title_endpoint(r)
        .or_else(|| r.get("navigationEndpoint"))
        .ok_or(())?;
    let browse_id = endpoint
        .pointer("/browseEndpoint/browseId")
        .and_then(Value::as_str)
        .or_else(|| {
            endpoint
                .pointer("/watchPlaylistEndpoint/playlistId")
                .and_then(Value::as_str)
        })
        .ok_or(())?;
    let source_id = browse_id.strip_prefix("VL").unwrap_or(browse_id);
    if source_id == "LM" {
        return Ok(None);
    }
    // Saved Episodes is a real system playlist, like LM, rather than an
    // unsupported utility tile. The current library card identifies it as SE
    // with an explicit playlist page type; keep unknown short IDs fail-closed.
    let saved_episodes = source_id == "SE"
        && endpoint.pointer("/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
            .and_then(Value::as_str) == Some("MUSIC_PAGE_TYPE_PLAYLIST");
    if !(saved_episodes
        || source_id.starts_with("PL")
        || source_id.starts_with("OLAK")
        || source_id.starts_with("RD"))
    {
        return Err(());
    }
    let title = r
        .get("title")
        .map(text)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| flex(r, 0));
    Ok(Some(LibraryEntry {
        source_id: source_id.into(),
        title,
        thumbnail: thumbnail(r),
        expected_count: None,
    }))
}

fn seconds(text: &str) -> f64 {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return 0.0;
    }
    parts
        .iter()
        .try_fold(0u64, |acc, part| {
            part.parse::<u64>().ok().map(|v| acc * 60 + v)
        })
        .unwrap_or(0) as f64
}

pub fn continuation(container: &Value) -> Option<String> {
    // Only look inside this result container, never a header's unrelated suggestions.
    container
        .get("continuations")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find_map(|v| find(v, "continuation").and_then(Value::as_str))
        })
        .or_else(|| {
            container
                .get("contents")
                .or_else(|| container.get("items"))
                .or_else(|| container.get("continuationItems"))
                .and_then(Value::as_array)
                .and_then(|items| {
                    items.iter().find_map(|v| {
                        v.get("continuationItemRenderer")
                            .and_then(|r| find(r, "token"))
                            .and_then(Value::as_str)
                    })
                })
        })
        .map(str::to_owned)
}

pub fn library_container(value: &Value) -> Option<&Value> {
    find(value, "gridRenderer")
        .or_else(|| find(value, "gridContinuation"))
        .or_else(|| find(value, "musicShelfRenderer"))
        .or_else(|| find(value, "musicShelfContinuation"))
}

pub fn library_entries(container: &Value) -> Vec<LibraryEntry> {
    container
        .get("items")
        .or_else(|| container.get("contents"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| library_entry(item).ok().flatten())
                .collect()
        })
        .unwrap_or_default()
}

pub fn unknown_library_rows(container: &Value) -> usize {
    container
        .get("items")
        .or_else(|| container.get("contents"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| library_entry(item).is_err())
                .count()
        })
        .unwrap_or(1)
}

pub fn playlist_container(value: &Value) -> Option<&Value> {
    find(value, "musicPlaylistShelfRenderer")
        .or_else(|| find(value, "musicPlaylistShelfContinuation"))
        .or_else(|| find(value, "musicShelfContinuation"))
        .or_else(|| find(value, "appendContinuationItemsAction"))
}

pub fn playlist_rows(
    container: &Value,
    source_id: &str,
    start: usize,
) -> (Vec<SourceTrack>, usize) {
    let Some(items) = container
        .get("contents")
        .or_else(|| container.get("continuationItems"))
        .and_then(Value::as_array)
    else {
        return (vec![], 1);
    };
    let mut unknown = 0;
    let mut rows = vec![];
    for item in items {
        if item.get("continuationItemRenderer").is_some() {
            continue;
        }
        let Some(renderer) = item.get("musicResponsiveListItemRenderer") else {
            unknown += 1;
            continue;
        };
        let playable_video_id = playback_endpoint(renderer)
            .and_then(|endpoint| endpoint.get("videoId"))
            .and_then(Value::as_str)
            .or_else(|| {
                renderer
                    .get("playlistItemData")
                    .and_then(|v| v.get("videoId"))
                    .and_then(Value::as_str)
            });
        let source_video_id = playable_video_id
            .or_else(|| {
                renderer
                    .get("menu")
                    .and_then(|menu| find(menu, "removedVideoId"))
                    .and_then(Value::as_str)
            })
            .map(str::to_owned);
        if source_video_id.is_none()
            && renderer
                .get("musicItemRendererDisplayPolicy")
                .and_then(Value::as_str)
                != Some("MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT")
            && renderer.get("isPlayable").and_then(Value::as_bool) != Some(false)
        {
            // Preserve the occurrence for review, but do not mislabel an unknown
            // playback schema as a successfully imported unavailable song.
            unknown += 1;
        }
        let available = playable_video_id.is_some()
            && renderer
                .get("musicItemRendererDisplayPolicy")
                .and_then(Value::as_str)
                != Some("MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT")
            && renderer.get("isPlayable").and_then(Value::as_bool) != Some(false);
        let position = start + rows.len();
        let title = flex(renderer, 0);
        let duration = renderer
            .get("fixedColumns")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
            .and_then(|v| find(v, "text"))
            .map(text)
            .unwrap_or_default();
        let track = Track {
            id: if available {
                source_video_id.clone().unwrap_or_default()
            } else {
                format!("unavailable:{source_id}:{position}")
            },
            title: if title.is_empty() {
                "Unavailable track".into()
            } else {
                title
            },
            artist: flex(renderer, 1),
            thumbnail: thumbnail(renderer),
            duration_secs: seconds(&duration),
            stream_url: None,
        };
        rows.push(SourceTrack {
            position,
            source_video_id,
            available,
            track,
        });
    }
    (rows, unknown)
}

pub fn playlist_title(response: &Value) -> Option<String> {
    find(response, "musicResponsiveHeaderRenderer")
        .or_else(|| find(response, "musicDetailHeaderRenderer"))
        .and_then(|r| r.get("title"))
        .map(text)
        .filter(|s| !s.is_empty())
}

pub fn expected_count(response: &Value) -> Option<usize> {
    let header = find(response, "musicResponsiveHeaderRenderer")
        .or_else(|| find(response, "musicDetailHeaderRenderer"))?;
    // Requests use English UI; accept only an explicit exact song count, not rounded 5K labels.
    fn count(value: &Value) -> Option<usize> {
        if let Some(runs) = value.get("runs").and_then(Value::as_array) {
            for run in runs {
                let label = run.get("text").and_then(Value::as_str).unwrap_or_default();
                if let Some(number) = label
                    .strip_suffix(" songs")
                    .or_else(|| label.strip_suffix(" song"))
                    .or_else(|| label.strip_suffix(" episodes"))
                    .or_else(|| label.strip_suffix(" episode"))
                    .or_else(|| label.strip_suffix(" tracks"))
                    .or_else(|| label.strip_suffix(" track"))
                {
                    let digits = number.replace([',', ' '], "");
                    if let Ok(result) = digits.parse() {
                        return Some(result);
                    }
                }
            }
        }
        match value {
            Value::Object(map) => map.values().find_map(count),
            Value::Array(items) => items.iter().find_map(count),
            _ => None,
        }
    }
    count(header)
}

fn panel_track(renderer: &Value) -> Option<Track> {
    let id = renderer.get("videoId").and_then(Value::as_str)?;
    if renderer.get("unplayableText").is_some() {
        return None;
    }
    Some(Track {
        id: id.into(),
        title: renderer.get("title").map(text).unwrap_or_default(),
        artist: renderer
            .get("longBylineText")
            .or_else(|| renderer.get("shortBylineText"))
            .map(text)
            .unwrap_or_default(),
        thumbnail: thumbnail(renderer),
        duration_secs: renderer
            .get("lengthText")
            .map(text)
            .map(|s| seconds(&s))
            .unwrap_or_default(),
        stream_url: None,
    })
}

pub fn radio_tracks(response: &Value) -> Vec<Track> {
    fn walk(value: &Value, result: &mut Vec<Track>) {
        if let Some(renderer) = value.get("playlistPanelVideoRenderer") {
            if let Some(track) = panel_track(renderer) {
                result.push(track);
            }
            return;
        }
        match value {
            Value::Object(map) => map.values().for_each(|v| walk(v, result)),
            Value::Array(items) => items.iter().for_each(|v| walk(v, result)),
            _ => (),
        }
    }
    let mut result = vec![];
    walk(response, &mut result);
    result
}

pub fn browse_tracks(response: &Value) -> Vec<Track> {
    fn walk(value: &Value, rows: &mut Vec<Track>) {
        if value.get("musicResponsiveListItemRenderer").is_some() {
            let (tracks, _) = playlist_rows(
                &serde_json::json!({"contents":[value]}),
                "browse",
                rows.len(),
            );
            rows.extend(
                tracks
                    .into_iter()
                    .filter(|track| track.available)
                    .map(|track| track.track),
            );
            return;
        }
        if let Some(item) = home_item(value) {
            if let Some(track) = item.track {
                rows.push(track);
            }
            return;
        }
        match value {
            Value::Object(map) => map.values().for_each(|v| walk(v, rows)),
            Value::Array(items) => items.iter().for_each(|v| walk(v, rows)),
            _ => (),
        }
    }
    let mut result = vec![];
    walk(response, &mut result);
    result
}

fn home_item(item: &Value) -> Option<HomeItem> {
    if let Some(r) = item.get("musicResponsiveListItemRenderer") {
        let (mut rows, _) = playlist_rows(&serde_json::json!({"contents":[item]}), "home", 0);
        let row = rows.pop()?;
        if !row.available {
            return None;
        }
        return Some(HomeItem {
            kind: "track".into(),
            id: row.track.id.clone(),
            title: row.track.title.clone(),
            subtitle: row.track.artist.clone(),
            thumbnail: thumbnail(r),
            track: Some(row.track),
        });
    }
    let r = item.get("musicTwoRowItemRenderer")?;
    let endpoint = title_endpoint(r).or_else(|| r.get("navigationEndpoint"))?;
    let video = if endpoint.get("browseEndpoint").is_some() {
        None
    } else {
        playback_endpoint(r)
            .and_then(|v| v.get("videoId"))
            .and_then(Value::as_str)
    };
    let watch_playlist = endpoint
        .pointer("/watchPlaylistEndpoint/playlistId")
        .and_then(Value::as_str);
    let id = video.or(watch_playlist).or_else(|| {
        endpoint
            .pointer("/browseEndpoint/browseId")
            .and_then(Value::as_str)
    })?;
    let kind = if video.is_some() {
        "track"
    } else if watch_playlist.is_some() || id.starts_with("VL") {
        "playlist"
    } else if id.starts_with("MPRE") {
        "album"
    } else if id.starts_with("UC") {
        "artist"
    } else {
        return None;
    };
    let title = r.get("title").map(text).unwrap_or_default();
    let subtitle = r.get("subtitle").map(text).unwrap_or_default();
    let thumbnail = thumbnail(r);
    let track = video.map(|id| Track {
        id: id.into(),
        title: title.clone(),
        artist: subtitle.clone(),
        thumbnail: thumbnail.clone(),
        duration_secs: 0.0,
        stream_url: None,
    });
    Some(HomeItem {
        kind: kind.into(),
        id: id.strip_prefix("VL").unwrap_or(id).into(),
        title,
        subtitle,
        thumbnail,
        track,
    })
}

pub fn home_sections(response: &Value) -> Vec<HomeSection> {
    fn walk(value: &Value, result: &mut Vec<HomeSection>) {
        if let Some(r) = value.get("musicCarouselShelfRenderer") {
            let title = r
                .get("header")
                .and_then(|v| find(v, "title"))
                .map(text)
                .unwrap_or_default();
            let items: Vec<HomeItem> = r
                .get("contents")
                .and_then(Value::as_array)
                .map(|v| v.iter().filter_map(home_item).collect())
                .unwrap_or_default();
            if !items.is_empty() {
                result.push(HomeSection { title, items });
            }
            return;
        }
        match value {
            Value::Object(map) => map.values().for_each(|v| walk(v, result)),
            Value::Array(items) => items.iter().for_each(|v| walk(v, result)),
            _ => (),
        }
    }
    let mut result = vec![];
    walk(response, &mut result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn row(id: Option<&str>, title: &str) -> Value {
        let mut result = json!({"musicResponsiveListItemRenderer":{"playlistItemData":{"videoId":id},"flexColumns":[{"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":title}]}}}]}});
        if id.is_none() {
            result["musicResponsiveListItemRenderer"]["musicItemRendererDisplayPolicy"] =
                json!("MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT");
        }
        result
    }
    #[test]
    fn keeps_duplicates_positions_and_unavailable() {
        let page = json!({"contents":[row(Some("same"),"First"),row(Some("same"),"First"),row(None,"Deleted"),{"continuationItemRenderer":{"continuationEndpoint":{"continuationCommand":{"token":"next"}}}}]});
        let (rows, unknown) = playlist_rows(&page, "PLtest", 10);
        assert_eq!(rows.len(), 3);
        assert_eq!(unknown, 0);
        assert_eq!(rows[0].track.id, rows[1].track.id);
        assert_eq!(rows[2].position, 12);
        assert!(!rows[2].available);
        assert_eq!(rows[2].track.title, "Deleted");
        assert_eq!(continuation(&page).as_deref(), Some("next"));
    }
    #[test]
    fn unknown_rows_are_counted_and_cannot_claim_completeness() {
        let (_, unknown) = playlist_rows(&json!({"contents":[{"newUnknownRenderer":{}}]}), "PL", 0);
        assert_eq!(unknown, 1);
    }
    #[test]
    fn extracts_only_exact_counts() {
        assert_eq!(
            expected_count(
                &json!({"musicResponsiveHeaderRenderer":{"subtitle":{"runs":[{"text":"1,234 songs"}]}}})
            ),
            Some(1234)
        );
        assert_eq!(
            expected_count(
                &json!({"musicResponsiveHeaderRenderer":{"subtitle":{"runs":[{"text":"5K songs"}]}}})
            ),
            None
        );
    }

    // Sanitized structural fixtures matching ytmusicapi 1.12.3 PLAY_BUTTON and
    // TITLE + NAVIGATION_BROWSE_ID paths, not an invented playlistItemData schema.
    #[test]
    fn current_overlay_play_button_is_the_primary_playlist_video_id() {
        let song = json!({"musicResponsiveListItemRenderer":{
            "overlay":{"musicItemThumbnailOverlayRenderer":{"content":{"musicPlayButtonRenderer":{"playNavigationEndpoint":{"watchEndpoint":{"videoId":"actualVideo1"}}}}}},
            "playlistItemData":{"videoId":"staleVideo2"},
            "flexColumns":[
                {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"A song"}]}}},
                {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"An artist"}]}}}
            ],
            "fixedColumns":[{"musicResponsiveListItemFixedColumnRenderer":{"text":{"runs":[{"text":"3:07"}]}}}]
        }});
        let (tracks, unknown) = playlist_rows(&json!({"contents":[song]}), "PLfixture", 0);
        assert_eq!(unknown, 0);
        assert_eq!(tracks.len(), 1);
        assert!(tracks[0].available);
        assert_eq!(tracks[0].source_video_id.as_deref(), Some("actualVideo1"));
        assert_eq!(tracks[0].track.id, "actualVideo1");
        assert_eq!(tracks[0].track.duration_secs, 187.0);
    }

    #[test]
    fn current_library_title_navigation_is_parsed_and_missing_destinations_are_unknown() {
        let container = json!({"items":[
            {"musicTwoRowItemRenderer":{"title":{"runs":[{"text":"A playlist","navigationEndpoint":{"browseEndpoint":{"browseId":"VLPLactual"}}}]}}},
            {"musicTwoRowItemRenderer":{"title":{"runs":[{"text":"Lost destination"}]}}},
            {"musicTwoRowItemRenderer":{"navigationEndpoint":{"modalEndpoint":{"modal":{"content":{"musicCreatePlaylistFormRenderer":{}}}}}}}
        ]});
        let entries = library_entries(&container);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source_id, "PLactual");
        assert_eq!(entries[0].title, "A playlist");
        assert_eq!(unknown_library_rows(&container), 1);
    }

    #[test]
    fn saved_episodes_is_inventory_not_an_unknown_or_silently_skipped_card() {
        let saved: Value =
            serde_json::from_str(include_str!("fixtures/library_saved_episodes.json")).unwrap();
        let mut items = vec![
            json!({"musicCreatePlaylistButtonRenderer":{}}),
            json!({"musicTwoRowItemRenderer":{"title":{"runs":[{"text":"Liked Music","navigationEndpoint":{"browseEndpoint":{"browseId":"VLLM"}}}]}}}),
        ];
        for index in 0..13 {
            items.push(json!({"musicTwoRowItemRenderer":{"title":{"runs":[{"text":"Synthetic playlist","navigationEndpoint":{"browseEndpoint":{"browseId":format!("VLPLfixture{index}")}}}]}}}));
        }
        items.push(saved.clone());
        let page = json!({"items":items});
        let entries = library_entries(&page);
        assert_eq!(
            entries.len(),
            14,
            "all 13 owned/saved playlists and SE must survive"
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.source_id == "SE")
                .count(),
            1
        );
        assert_eq!(unknown_library_rows(&page), 0);
        assert!(continuation(&page).is_none());
        let mut unknown = saved.clone();
        unknown["musicTwoRowItemRenderer"]["title"]["runs"][0]["navigationEndpoint"]
            ["browseEndpoint"]["browseId"] = json!("VLXY");
        assert_eq!(unknown_library_rows(&json!({"items":[unknown]})), 1);
        let mut wrong_page = saved;
        wrong_page["musicTwoRowItemRenderer"]["title"]["runs"][0]["navigationEndpoint"]
            ["browseEndpoint"]["browseEndpointContextSupportedConfigs"]
            ["browseEndpointContextMusicConfig"]["pageType"] = json!("MUSIC_PAGE_TYPE_ALBUM");
        assert_eq!(unknown_library_rows(&json!({"items":[wrong_page]})), 1);
    }

    #[test]
    fn saved_episode_counts_are_exact_and_an_empty_verified_playlist_is_complete() {
        for (label, count) in [
            ("0 episodes", Some(0)),
            ("1 episode", Some(1)),
            ("1,234 episodes", Some(1234)),
            ("5K episodes", None),
        ] {
            assert_eq!(
                expected_count(
                    &json!({"musicResponsiveHeaderRenderer":{"subtitle":{"runs":[{"text":label}]}}})
                ),
                count
            );
        }
        let entry = LibraryEntry {
            source_id: "SE".into(),
            title: "Saved Episodes".into(),
            thumbnail: String::new(),
            expected_count: None,
        };
        let mut collector = PlaylistCollector::new(entry);
        assert!(collector.consume(&json!({"musicResponsiveHeaderRenderer":{"subtitle":{"runs":[{"text":"0 episodes"}]}}})).unwrap().is_none());
        let playlist = collector.finish();
        assert!(playlist.complete);
        assert_eq!(playlist.source.expected_count, Some(0));
        assert!(playlist.tracks.is_empty());
    }

    #[test]
    fn current_zero_episode_empty_shelf_is_complete_but_unknown_zero_count_rows_are_not() {
        let fixture: Value =
            serde_json::from_str(include_str!("fixtures/empty_saved_episodes.json")).unwrap();
        let source = LibraryEntry {
            source_id: "SE".into(),
            title: String::new(),
            thumbnail: String::new(),
            expected_count: None,
        };
        let mut state = PlaylistCollector::new(source.clone());
        assert!(state.consume(&fixture).unwrap().is_none());
        let result = state.finish();
        assert!(result.complete);
        assert_eq!(result.source.expected_count, Some(0));
        assert!(result.warnings.is_empty());
        for (key, value) in [
            ("contents", json!([{"newUnknownRenderer":{}}])),
            ("newRows", json!([{}])),
        ] {
            let mut unknown = fixture.clone();
            unknown["musicPlaylistShelfRenderer"][key] = value;
            let mut state = PlaylistCollector::new(source.clone());
            state.consume(&unknown).unwrap();
            assert!(
                !state.finish().complete,
                "zero count must not hide unknown rows"
            );
        }
        let mut unknown = fixture;
        unknown["musicPlaylistShelfRenderer"]["emptyState"] = json!({"newEmptyRenderer":{}});
        let mut state = PlaylistCollector::new(source);
        state.consume(&unknown).unwrap();
        assert!(!state.finish().complete);
    }

    #[test]
    fn current_tracks_label_is_an_exact_playlist_count() {
        assert_eq!(
            expected_count(
                &json!({"musicResponsiveHeaderRenderer":{"secondSubtitle":{"runs":[{"text":"29 tracks"}]}}})
            ),
            Some(29)
        );
        assert_eq!(
            expected_count(
                &json!({"musicResponsiveHeaderRenderer":{"secondSubtitle":{"runs":[{"text":"5K tracks"}]}}})
            ),
            None
        );
    }

    #[test]
    fn home_album_playlist_and_artist_follow_title_navigation_even_with_play_overlay() {
        for (browse_id, kind, expected_id) in [
            ("VLPLfixture", "playlist", "PLfixture"),
            ("MPREb_fixture", "album", "MPREb_fixture"),
            ("UCfixture", "artist", "UCfixture"),
        ] {
            let item = json!({"musicTwoRowItemRenderer":{
                "title":{"runs":[{"text":"Recommendation","navigationEndpoint":{"browseEndpoint":{"browseId":browse_id}}}]},
                "overlay":{"musicItemThumbnailOverlayRenderer":{"content":{"musicPlayButtonRenderer":{"playNavigationEndpoint":{"watchEndpoint":{"videoId":"firstSong01"}}}}}}
            }});
            let parsed = home_item(&item).unwrap();
            assert_eq!(parsed.kind, kind);
            assert_eq!(parsed.id, expected_id);
            assert!(parsed.track.is_none());
        }
    }

    #[test]
    fn unknown_song_endpoint_is_retained_but_cannot_claim_complete() {
        let value = json!({"musicResponsiveListItemRenderer":{"flexColumns":[{"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Unrecognized playback schema"}]}}}]}});
        let mut state = collector(Some(1));
        state.consume(&page(vec![value], None)).unwrap();
        let result = state.finish();
        assert_eq!(result.tracks.len(), 1);
        assert!(!result.complete);
    }

    fn collector(expected: Option<usize>) -> PlaylistCollector {
        PlaylistCollector::new(LibraryEntry {
            source_id: "PL-test".into(),
            title: "Test".into(),
            thumbnail: String::new(),
            expected_count: expected,
        })
    }

    fn page(contents: Vec<Value>, token: Option<&str>) -> Value {
        let mut shelf = json!({"contents":contents});
        if let Some(token) = token {
            shelf["continuations"] = json!([{"nextContinuationData":{"continuation":token}}]);
        }
        json!({"continuationContents":{"musicPlaylistShelfContinuation":shelf}})
    }

    #[test]
    fn pagination_reads_more_than_five_thousand_without_deduplicating_occurrences() {
        let mut state = collector(Some(6001));
        for index in 0..3 {
            let token = format!("page-{}", index + 1);
            let rows = (0..2000).map(|_| row(Some("same"), "Song")).collect();
            assert_eq!(
                state.consume(&page(rows, Some(&token))).unwrap(),
                Some(token)
            );
        }
        assert!(state
            .consume(&page(vec![row(None, "Unavailable")], None))
            .unwrap()
            .is_none());
        let result = state.finish();
        assert!(result.complete);
        assert_eq!(result.tracks.len(), 6001);
        assert_eq!(result.tracks[6000].position, 6000);
        assert!(!result.tracks[6000].available);
    }

    #[test]
    fn pagination_failure_returns_partial_and_count_mismatch() {
        let mut state = collector(Some(3));
        state
            .consume(&page(vec![row(Some("first"), "First")], Some("next")))
            .unwrap();
        state.failure("Network request failed".into()).unwrap();
        let result = state.finish();
        assert!(!result.complete);
        assert_eq!(result.tracks.len(), 1);
        assert!(result
            .warnings
            .iter()
            .any(|w| w.contains("Source reports 3")));
        assert!(collector(Some(3)).failure("No first page".into()).is_err());
    }

    #[test]
    fn repeated_continuation_and_unknown_count_never_claim_complete() {
        let mut state = collector(None);
        state
            .consume(&page(vec![row(Some("one"), "One")], Some("repeated")))
            .unwrap();
        assert!(state
            .consume(&page(vec![row(Some("two"), "Two")], Some("repeated")))
            .unwrap()
            .is_none());
        let result = state.finish();
        assert!(!result.complete);
        assert_eq!(result.tracks.len(), 2);
        assert!(result.warnings.iter().any(|w| w.contains("repeated")));
        assert!(result.warnings.iter().any(|w| w.contains("exact total")));
    }
}
