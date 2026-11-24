//! Public YouTube search through the YouTube Data API v3.
use super::SearchFilter;
use crate::api::{http, types::*};
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const ENDPOINT: &str = "https://www.googleapis.com/youtube/v3/search";
pub(super) const TOKEN_PREFIX: &str = "youtube-data-v3:";

pub(super) fn api_key() -> Option<String> {
    std::env::var("STREAMTUBE_YOUTUBE_API_KEY")
        .ok()
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
}

#[derive(Serialize, Deserialize)]
struct SearchRequest {
    parameters: Vec<(String, String)>,
    page_token: Option<String>,
}

pub(super) fn search(key: &str, query: &str, filter: &SearchFilter) -> Result<Page> {
    let (language, region) = super::region();
    let video_filter = filter.kind == 4 || filter.duration > 0 || filter.hd
        || filter.subtitles || filter.creative_commons || filter.live;
    let kind = match filter.kind {
        2 => "channel",
        3 => "playlist",
        1 | 4 => "video",
        _ if video_filter => "video",
        _ => "video,channel,playlist",
    };
    if video_filter && matches!(filter.kind, 2 | 3) {
        bail!("YouTube Data API video filters require video results");
    }
    let mut parameters = vec![
        ("part", "snippet".to_owned()),
        ("maxResults", "50".to_owned()),
        ("q", query.to_owned()),
        ("type", kind.to_owned()),
        ("order", match filter.sort {
            1 => "rating",
            2 => "date",
            3 => "viewCount",
            _ => "relevance",
        }.to_owned()),
    ];
    if !language.is_empty() {
        parameters.push(("relevanceLanguage", language));
    }
    if region.len() == 2 && region.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        parameters.push(("regionCode", region));
    }
    if let Some(age) = match filter.date {
        1 => Some(Duration::hours(1)),
        2 => Some(Duration::days(1)),
        3 => Some(Duration::weeks(1)),
        4 => Some(Duration::days(30)),
        5 => Some(Duration::days(365)),
        _ => None,
    } {
        parameters.push(("publishedAfter", (Utc::now() - age)
            .to_rfc3339_opts(SecondsFormat::Secs, true)));
    }
    for (name, value) in [
        ("videoType", (filter.kind == 4).then_some("movie")),
        ("videoDuration", match filter.duration {
            1 => Some("short"),
            2 => Some("long"),
            3 => Some("medium"),
            _ => None,
        }),
        ("videoDefinition", filter.hd.then_some("high")),
        ("videoCaption", filter.subtitles.then_some("closedCaption")),
        ("videoLicense", filter.creative_commons.then_some("creativeCommon")),
        ("eventType", filter.live.then_some("live")),
    ] {
        if let Some(value) = value {
            parameters.push((name, value.to_owned()));
        }
    }
    let request = SearchRequest {
        parameters: parameters.into_iter().map(|(name, value)| (name.to_owned(), value)).collect(),
        page_token: None,
    };
    fetch_page(key, request)
}

pub(super) fn search_continue(token: &str) -> Result<Page> {
    let key = api_key().context("Set STREAMTUBE_YOUTUBE_API_KEY to continue this search")?;
    let request = serde_json::from_str(token).context("Invalid YouTube search continuation")?;
    fetch_page(&key, request)
}

fn fetch_page(key: &str, mut request: SearchRequest) -> Result<Page> {
    let mut query: Vec<_> = request.parameters.iter()
        .map(|(name, value)| format!("{}={}", http::urlencode(name), http::urlencode(value)))
        .collect();
    if let Some(token) = &request.page_token {
        query.push(format!("pageToken={}", http::urlencode(token)));
    }
    let url = format!("{ENDPOINT}?{}", query.join("&"));
    let (status, response) = http::get_json_status_with_headers(&url, &[("X-Goog-Api-Key", key)])
        .map_err(|_| anyhow!("Could not read the YouTube Data API response"))?;
    if status != 200 {
        let hint = match status {
            400 => "check the API key and search filters",
            401 | 403 => "check that YouTube Data API v3 is enabled, the key is allowed, and quota is available",
            429 => "try again later or check the project quota",
            _ => "try again later",
        };
        bail!("YouTube Data API returned HTTP {status}: {hint}");
    }
    let items = response["items"].as_array()
        .context("YouTube Data API search response is missing its results")?
        .iter().filter_map(parse_item).collect();
    request.page_token = response["nextPageToken"].as_str().map(str::to_owned);
    let continuation = if request.page_token.is_some() {
        Some(format!("{TOKEN_PREFIX}{}", serde_json::to_string(&request)?))
    } else {
        None
    };
    Ok(Page { items, continuation })
}

fn parse_item(value: &Value) -> Option<Item> {
    let snippet = value.get("snippet")?;
    let title = snippet["title"].as_str()?.to_owned();
    let channel_id = snippet["channelId"].as_str().map(str::to_owned);
    let channel_name = snippet["channelTitle"].as_str().map(str::to_owned);
    let description = snippet["description"].as_str().map(str::to_owned);
    let thumbnail = ["high", "medium", "default"].iter()
        .find_map(|size| snippet["thumbnails"][*size]["url"].as_str())
        .map(str::to_owned);
    let id = &value["id"];
    match id["kind"].as_str()? {
        "youtube#video" => {
            let id = id["videoId"].as_str()?.to_owned();
            let published_text = snippet["publishedAt"].as_str().map(str::to_owned);
            let published_ts = published_text.as_deref()
                .and_then(|date| DateTime::parse_from_rfc3339(date).ok())
                .map(|date| date.timestamp());
            Some(Item::Video(VideoItem {
                thumbnail: thumbnail.unwrap_or_else(|| super::default_thumb(&id)),
                id, title, channel_id, published_text, published_ts,
                channel_name: channel_name.unwrap_or_default(),
                description_snippet: description,
                is_live: snippet["liveBroadcastContent"] == "live",
                is_upcoming: snippet["liveBroadcastContent"] == "upcoming",
                ..VideoItem::default()
            }))
        }
        "youtube#channel" => Some(Item::Channel(ChannelItem {
            id: id["channelId"].as_str()?.to_owned(),
            name: title,
            avatar: thumbnail,
            description,
            ..ChannelItem::default()
        })),
        "youtube#playlist" => Some(Item::Playlist(PlaylistItem {
            id: id["playlistId"].as_str()?.to_owned(),
            title, thumbnail, channel_name, channel_id,
            ..PlaylistItem::default()
        })),
        _ => None,
    }
}
