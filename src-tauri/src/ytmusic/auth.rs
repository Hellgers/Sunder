use super::types::{ConnectionStatus, LoginChallenge, LoginProgress};
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Credentials belong to the user's own Cloud project. No shared client identity.
const DEVICE_URL: &str = "https://oauth2.googleapis.com/device/code";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/youtube";

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Keep secrets out of the checkout even if Tauri falls back to its working directory.
pub fn credential_path() -> PathBuf {
    #[cfg(windows)]
    let directory = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Sunder")
        .join("YouTubeMusic");
    #[cfg(not(windows))]
    let directory = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(std::env::temp_dir)
        .join("sunder")
        .join("ytmusic");
    directory.join("credentials.bin")
}

#[derive(Default, Serialize, Deserialize)]
struct Credentials {
    client_id: String,
    client_secret: String,
    access_token: String,
    refresh_token: String,
    expires_at: u64,
    #[serde(default)]
    namespace: String,
}

struct PendingLogin {
    device_code: String,
    expires_at: u64,
    next_poll_at: u64,
    interval: u64,
}

pub struct Auth {
    path: PathBuf,
    credentials: Credentials,
    pending: Option<PendingLogin>,
    browser: Option<BrowserCredentials>,
    browser_generation: u64,
}

impl Auth {
    pub fn new(path: PathBuf) -> Self {
        let credentials = std::fs::read(&path)
            .ok()
            .and_then(|bytes| unprotect(&bytes).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let browser = std::fs::read(path.with_file_name("browser-credentials.bin"))
            .ok()
            .filter(|bytes| bytes.len() <= MAX_BROWSER_BYTES as usize + 4096)
            .and_then(|bytes| unprotect(&bytes).ok())
            .and_then(|bytes| serde_json::from_slice::<BrowserCredentials>(&bytes).ok())
            .filter(|browser| browser.is_verified());
        Self {
            path,
            credentials,
            pending: None,
            browser,
            browser_generation: 0,
        }
    }

    pub fn status(&self) -> ConnectionStatus {
        let oauth_connected = !self.credentials.refresh_token.is_empty()
            || (!self.credentials.access_token.is_empty() && self.credentials.expires_at > now());
        ConnectionStatus {
            configured: self.browser.is_some() || !self.credentials.client_id.is_empty(),
            connected: self.browser.as_ref().is_some_and(|browser| browser.usable),
            auth_method: if self.browser.is_some() {
                "browser"
            } else if !self.credentials.client_id.is_empty() {
                "oauth"
            } else {
                "none"
            },
            identity_label: self
                .browser
                .as_ref()
                .map(|browser| browser.channel_id.clone()),
            oauth_connected,
        }
    }

    pub fn browser_headers(&self) -> Result<Option<(u64, HeaderMap)>, String> {
        if self.browser.as_ref().is_some_and(|browser| !browser.usable) {
            return Err(BROWSER_EXPIRED_ERROR.into());
        }
        self.browser
            .as_ref()
            .map(|browser| {
                browser
                    .headers(now())
                    .map(|headers| (self.browser_generation, headers))
            })
            .transpose()
    }

    pub fn browser_namespace(&self) -> Result<String, String> {
        self.browser
            .as_ref()
            .map(|browser| format!("youtube-channel:{}", browser.channel_id))
            .ok_or_else(|| {
                "Connect a browser session before importing your YouTube Music library".into()
            })
    }

    pub fn save_browser(&mut self, browser: BrowserCredentials) -> Result<(), String> {
        if !browser.is_verified() || !browser.usable {
            return Err("Could not verify browser channel identity".into());
        }
        self.persist_browser(&browser)?;
        self.browser = Some(browser);
        self.browser_generation = self.browser_generation.wrapping_add(1);
        Ok(())
    }

    pub fn invalidate_browser(&mut self, generation: u64) -> Result<(), String> {
        if generation != self.browser_generation {
            return Ok(());
        }
        let Some(browser) = self.browser.as_mut() else {
            return Ok(());
        };
        browser.usable = false;
        self.persist_browser(self.browser.as_ref().unwrap())
    }

    fn persist_browser(&self, browser: &BrowserCredentials) -> Result<(), String> {
        let path = self.path.with_file_name("browser-credentials.bin");
        let parent = path.parent().ok_or("Invalid browser credential storage")?;
        std::fs::create_dir_all(parent)
            .map_err(|_| "Could not create browser credential storage")?;
        let bytes =
            serde_json::to_vec(&browser).map_err(|_| "Could not encode browser credentials")?;
        let protected = protect(&bytes)?;
        let temporary = path.with_extension("tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        let mut file = options
            .open(&temporary)
            .map_err(|_| "Could not save browser credentials")?;
        file.write_all(&protected)
            .and_then(|_| file.sync_all())
            .map_err(|_| "Could not save browser credentials")?;
        drop(file);
        #[cfg(windows)]
        encryption::replace_file(&temporary, &path)?;
        #[cfg(not(windows))]
        std::fs::rename(&temporary, &path)
            .map_err(|_| "Could not finish saving browser credentials")?;
        Ok(())
    }

    #[cfg(test)]
    pub fn anonymous() -> Self {
        Self {
            path: PathBuf::new(),
            credentials: Credentials::default(),
            pending: None,
            browser: None,
            browser_generation: 0,
        }
    }

    #[allow(dead_code)] // Retained for the official Data API; Music never binds to this account implicitly.
    pub async fn namespace(&mut self, client: &Client) -> Result<String, String> {
        // Reconnects and OAuth client changes must resolve to the same immutable
        // YouTube channel, rather than a process ID, mutable handle or email address.
        let token = self.token(client).await?;
        let response = client
            .get("https://www.googleapis.com/youtube/v3/channels")
            .query(&[
                ("part", "id"),
                ("mine", "true"),
                ("maxResults", "50"),
                ("fields", "items(id),nextPageToken,pageInfo(totalResults)"),
            ])
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| "Could not identify the connected YouTube channel")?;
        if !response.status().is_success() {
            return Err("Google could not identify the connected YouTube channel; check authorization and YouTube Data API access".into());
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| "Could not read YouTube channel identity")?;
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|_| "Google returned an invalid channel identity response")?;
        let namespace = channel_namespace(&body)?;
        if self.credentials.namespace != namespace {
            self.credentials.namespace = namespace.clone();
            self.persist()?;
        }
        Ok(namespace)
    }

    pub fn disconnect(&mut self) -> Result<(), String> {
        self.credentials = Credentials::default();
        self.pending = None;
        self.browser = None;
        match std::fs::remove_file(self.path.with_file_name("browser-credentials.bin")) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err("Could not remove local browser credentials".into()),
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("Could not remove local YouTube Music credentials".into()),
        }
    }

    fn persist(&self) -> Result<(), String> {
        let parent = self
            .path
            .parent()
            .ok_or("Invalid credential storage directory")?;
        std::fs::create_dir_all(parent)
            .map_err(|_| "Could not create credential storage directory")?;
        let bytes =
            serde_json::to_vec(&self.credentials).map_err(|_| "Could not encode credentials")?;
        let protected = protect(&bytes)?;
        let temp = self.path.with_extension("tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        let mut file = options
            .open(&temp)
            .map_err(|_| "Could not open credential storage")?;
        file.write_all(&protected)
            .and_then(|_| file.sync_all())
            .map_err(|_| "Could not save credentials")?;
        drop(file);
        // Windows rename cannot overwrite an existing destination.
        if self.path.exists() {
            std::fs::remove_file(&self.path).map_err(|_| "Could not replace stored credentials")?;
        }
        std::fs::rename(temp, &self.path)
            .map_err(|_| "Could not finish saving credentials".to_string())
    }

    pub async fn begin(
        &mut self,
        client: &Client,
        client_id: String,
        client_secret: String,
    ) -> Result<LoginChallenge, String> {
        let client_id = client_id.trim().to_owned();
        let client_secret = client_secret.trim().to_owned();
        if !client_id.ends_with(".apps.googleusercontent.com") || client_secret.is_empty() {
            return Err("Provide the client ID and secret of your own Google Cloud TV/Limited Input OAuth client".into());
        }
        let body = oauth_post(
            client,
            DEVICE_URL,
            &[("client_id", &client_id), ("scope", SCOPE)],
        )
        .await?;
        if body.get("error").is_some() {
            return Err(oauth_error(&body));
        }
        let device_code = required_string(&body, "device_code")?;
        let user_code = required_string(&body, "user_code")?;
        let verification_url = body
            .get("verification_url")
            .or_else(|| body.get("verification_uri"))
            .and_then(Value::as_str)
            .ok_or("Google did not return a verification URL")?
            .to_owned();
        let url = reqwest::Url::parse(&verification_url)
            .map_err(|_| "Invalid Google verification URL")?;
        if url.scheme() != "https"
            || !matches!(
                url.host_str(),
                Some(
                    "www.google.com"
                        | "google.com"
                        | "accounts.google.com"
                        | "www.youtube.com"
                        | "youtube.com"
                )
            )
        {
            return Err("Unexpected OAuth verification host".into());
        }
        let expires_in = body
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(1800);
        let interval = body
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .max(1);
        self.credentials = Credentials {
            client_id,
            client_secret,
            ..Default::default()
        };
        self.pending = Some(PendingLogin {
            device_code,
            expires_at: now() + expires_in,
            next_poll_at: now() + interval,
            interval,
        });
        Ok(LoginChallenge {
            user_code,
            verification_url,
            expires_in,
            interval,
        })
    }

    pub async fn poll(&mut self, client: &Client) -> Result<LoginProgress, String> {
        if self.pending.is_none() && self.status().oauth_connected {
            return Ok(LoginProgress {
                status: "connected".into(),
                interval: 0,
            });
        }
        let pending = self
            .pending
            .as_mut()
            .ok_or("Start YouTube Music sign-in first")?;
        if now() >= pending.expires_at {
            self.pending = None;
            return Err("YouTube Music sign-in code expired; start again".into());
        }
        if now() < pending.next_poll_at {
            return Ok(LoginProgress {
                status: "pending".into(),
                interval: pending.next_poll_at - now(),
            });
        }
        pending.next_poll_at = now() + pending.interval;
        let body = oauth_post(
            client,
            TOKEN_URL,
            &[
                ("client_id", &self.credentials.client_id),
                ("client_secret", &self.credentials.client_secret),
                ("device_code", &pending.device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
        )
        .await?;
        match body.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => Ok(LoginProgress {
                status: "pending".into(),
                interval: pending.interval,
            }),
            Some("slow_down") => {
                pending.interval += 5;
                pending.next_poll_at = now() + pending.interval;
                Ok(LoginProgress {
                    status: "slow_down".into(),
                    interval: pending.interval,
                })
            }
            Some(_) => {
                self.pending = None;
                Err(oauth_error(&body))
            }
            None => {
                self.credentials.access_token = required_string(&body, "access_token")?;
                self.credentials.refresh_token = required_string(&body, "refresh_token")?;
                self.credentials.expires_at = now()
                    + body
                        .get("expires_in")
                        .and_then(Value::as_u64)
                        .unwrap_or(3600);
                self.persist()?;
                self.pending = None;
                Ok(LoginProgress {
                    status: "connected".into(),
                    interval: 0,
                })
            }
        }
    }

    pub async fn token(&mut self, client: &Client) -> Result<String, String> {
        if !self.credentials.access_token.is_empty() && self.credentials.expires_at > now() + 60 {
            return Ok(self.credentials.access_token.clone());
        }
        if self.credentials.refresh_token.is_empty() {
            return Err("Connect your YouTube Music account first".into());
        }
        let body = oauth_post(
            client,
            TOKEN_URL,
            &[
                ("client_id", &self.credentials.client_id),
                ("client_secret", &self.credentials.client_secret),
                ("refresh_token", &self.credentials.refresh_token),
                ("grant_type", "refresh_token"),
            ],
        )
        .await?;
        if body.get("error").is_some() {
            self.credentials.access_token.clear();
            return Err(oauth_error(&body));
        }
        self.credentials.access_token = required_string(&body, "access_token")?;
        self.credentials.expires_at = now()
            + body
                .get("expires_in")
                .and_then(Value::as_u64)
                .unwrap_or(3600);
        self.persist()?;
        Ok(self.credentials.access_token.clone())
    }
}

fn channel_namespace(body: &Value) -> Result<String, String> {
    let items = body
        .get("items")
        .and_then(Value::as_array)
        .filter(|items| items.len() == 1)
        .filter(|_| body.get("nextPageToken").is_none())
        .ok_or("Select one YouTube channel during Google sign-in before importing your library")?;
    let id = items[0]
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| {
            id.len() == 24
                && id.starts_with("UC")
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
        .ok_or("Google returned no usable YouTube channel identity")?;
    Ok(format!("youtube-channel:{id}"))
}

async fn oauth_post(client: &Client, url: &str, form: &[(&str, &str)]) -> Result<Value, String> {
    let response = client
        .post(url)
        .form(form)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| "Could not reach Google's OAuth service")?;
    let bytes = response
        .bytes()
        .await
        .map_err(|_| "Could not read Google's OAuth response")?;
    serde_json::from_slice(&bytes).map_err(|_| "Google returned an invalid OAuth response".into())
}

fn required_string(body: &Value, name: &str) -> Result<String, String> {
    body.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "Google returned an incomplete OAuth response".into())
}

fn oauth_error(body: &Value) -> String {
    // Never include response payloads: a response may contain credentials or tokens.
    match body.get("error").and_then(Value::as_str) {
        Some("access_denied") => "Google sign-in was declined",
        Some("expired_token") => "Google sign-in code expired; start again",
        Some("invalid_client" | "unauthorized_client") => "Check your own OAuth TV/Limited Input client and enable YouTube Data API v3 in its Cloud project",
        Some("invalid_grant") => "Google authorization expired or was revoked; reconnect your account",
        Some("invalid_scope") => "The OAuth client does not allow the required YouTube scope",
        _ => "Google could not authorize YouTube Music",
    }.into()
}

const MAX_BROWSER_BYTES: u64 = 128 * 1024;
pub const MUSIC_ORIGIN: &str = "https://music.youtube.com";
pub const MUSIC_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:88.0) Gecko/20100101 Firefox/88.0";
pub const BROWSER_EXPIRED_ERROR: &str =
    "The Music browser session expired or was denied; import fresh request headers";
pub const BROWSER_SIGNED_OUT_ERROR: &str =
    "The Music browser session is signed out; import fresh request headers";
const HEADER_ERROR: &str = "Choose a valid YouTube Music request-header file (JSON, raw headers, or Copy as fetch); no script is executed";

// Never derive Debug or return this object through IPC: it contains a browser session.
#[derive(Serialize, Deserialize)]
pub struct BrowserCredentials {
    cookie: String,
    authuser: String,
    pub channel_id: String,
    #[serde(default)]
    usable: bool,
}

pub fn valid_channel_id(id: &str) -> bool {
    id.len() == 24
        && id.starts_with("UC")
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn music_url(url: &str, origin_only: bool) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("music.youtube.com")
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && (!origin_only
                || (url.path() == "/" && url.query().is_none() && url.fragment().is_none()))
    })
}

fn sapisid(cookie: &str) -> Result<&str, String> {
    if cookie.is_empty() || !cookie.bytes().all(|b| (32..=126).contains(&b)) {
        return Err("The header file has an invalid cookie value".into());
    }
    let mut plain = None;
    let mut secure = None;
    for pair in cookie.split(';') {
        let Some((name, value)) = pair.trim().split_once('=') else {
            continue;
        };
        let slot = match name {
            "SAPISID" => &mut plain,
            "__Secure-3PAPISID" => &mut secure,
            _ => continue,
        };
        if value.is_empty() || slot.is_some() {
            return Err("The header file has ambiguous authentication cookies".into());
        }
        *slot = Some(value);
    }
    plain.or(secure).ok_or_else(|| {
        "The selected headers do not contain a signed-in YouTube Music session".into()
    })
}

impl BrowserCredentials {
    pub fn from_file(path: &Path) -> Result<Self, String> {
        if !path.is_absolute() || path.to_string_lossy().starts_with(r"\\") {
            return Err("Choose a local header file using the file picker".into());
        }
        let file =
            std::fs::File::open(path).map_err(|_| "Could not open the selected header file")?;
        let metadata = file
            .metadata()
            .map_err(|_| "Could not inspect the selected header file")?;
        if !metadata.is_file() || metadata.len() > MAX_BROWSER_BYTES {
            return Err(
                "The selected header file must be a regular file no larger than 128 KiB".into(),
            );
        }
        let mut bytes = Vec::new();
        file.take(MAX_BROWSER_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Could not read the selected header file")?;
        if bytes.len() > MAX_BROWSER_BYTES as usize {
            return Err("The selected header file exceeds 128 KiB".into());
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| "Save the header file as UTF-8 text")?;
        Self::parse(text)
    }

    fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim_start_matches('\u{feff}').trim();
        if text.len() > MAX_BROWSER_BYTES as usize {
            return Err("The selected header file exceeds 128 KiB".into());
        }
        let value = if text.starts_with('{') {
            Some(serde_json::from_str::<Value>(text).map_err(|_| HEADER_ERROR)?)
        } else if text.starts_with("fetch(") || text.starts_with("await fetch(") {
            let rest = text
                .strip_prefix("await ")
                .unwrap_or(text)
                .strip_prefix("fetch(")
                .ok_or(HEADER_ERROR)?
                .trim_start();
            let mut url_stream = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
            let url = url_stream
                .next()
                .ok_or(HEADER_ERROR)?
                .map_err(|_| HEADER_ERROR)?;
            if !url.as_str().is_some_and(|url| music_url(url, false)) {
                return Err("Only HTTPS YouTube Music request exports are accepted".into());
            }
            let rest = rest[url_stream.byte_offset()..]
                .trim_start()
                .strip_prefix(',')
                .ok_or(HEADER_ERROR)?
                .trim_start();
            let mut options_stream = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
            let options = options_stream
                .next()
                .ok_or(HEADER_ERROR)?
                .map_err(|_| HEADER_ERROR)?;
            if !matches!(rest[options_stream.byte_offset()..].trim(), ")" | ");") {
                return Err(HEADER_ERROR.into());
            }
            Some(options)
        } else {
            None
        };
        let mut entries = Vec::<(String, String)>::new();
        if let Some(value) = value {
            let outer = value.as_object().ok_or(HEADER_ERROR)?;
            for key in ["url", "referrer"] {
                if let Some(value) = outer.get(key) {
                    if !value.as_str().is_some_and(|url| music_url(url, false)) {
                        return Err("Only HTTPS YouTube Music request exports are accepted".into());
                    }
                }
            }
            let headers = outer
                .get("headers")
                .unwrap_or(&value)
                .as_object()
                .ok_or(HEADER_ERROR)?;
            for (key, value) in headers {
                if let Some(value) = value.as_str() {
                    entries.push((key.to_ascii_lowercase(), value.to_owned()));
                }
            }
        } else {
            let mut pending = None;
            for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
                if let Some(key) = pending.take() {
                    entries.push((key, line.to_owned()));
                    continue;
                }
                if let Some((key, value)) = line.trim_start_matches(':').split_once(':') {
                    if value.trim().is_empty() {
                        pending = Some(key.to_ascii_lowercase());
                    } else {
                        entries.push((key.to_ascii_lowercase(), value.trim().to_owned()));
                    }
                } else if matches!(
                    line.to_ascii_lowercase().as_str(),
                    "cookie" | "x-goog-authuser" | "origin" | "x-origin" | "referer"
                ) {
                    pending = Some(line.to_ascii_lowercase());
                }
            }
        }
        let mut cookie = None;
        let mut authuser = None;
        for (key, value) in entries {
            match key.as_str() {
                "origin" | "x-origin" if !music_url(&value, true) => {
                    return Err("The header origin must be https://music.youtube.com".into())
                }
                "referer" if !music_url(&value, false) => {
                    return Err("The header origin must be https://music.youtube.com".into())
                }
                "host" | "authority" if value != "music.youtube.com" => {
                    return Err("The header host must be music.youtube.com".into())
                }
                "cookie" if cookie.is_some() => return Err(HEADER_ERROR.into()),
                "cookie" => cookie = Some(value),
                "x-goog-authuser" if authuser.is_some() => return Err(HEADER_ERROR.into()),
                "x-goog-authuser" => authuser = Some(value),
                _ => (), // Imported Authorization, URLs, browser metadata and script bodies are never used.
            }
        }
        let cookie = cookie.ok_or("The selected file has no Cookie request header")?;
        sapisid(&cookie)?;
        let authuser = authuser.ok_or("The selected file has no x-goog-authuser request header")?;
        if authuser.is_empty()
            || authuser.len() > 3
            || !authuser.bytes().all(|c| c.is_ascii_digit())
        {
            return Err("The selected file has an invalid account selector".into());
        }
        Ok(Self {
            cookie,
            authuser,
            channel_id: String::new(),
            usable: false,
        })
    }

    pub fn set_verified_channel(&mut self, channel_id: String) -> Result<(), String> {
        if !valid_channel_id(&channel_id) {
            return Err("Could not verify browser channel identity".into());
        }
        self.channel_id = channel_id;
        self.usable = true;
        Ok(())
    }

    fn is_verified(&self) -> bool {
        valid_channel_id(&self.channel_id)
            && sapisid(&self.cookie).is_ok()
            && !self.authuser.is_empty()
            && self.authuser.len() <= 3
            && self.authuser.bytes().all(|c| c.is_ascii_digit())
    }

    pub fn headers(&self, timestamp: u64) -> Result<HeaderMap, String> {
        use sha1::{Digest, Sha1};
        let digest = Sha1::digest(
            format!("{timestamp} {} {MUSIC_ORIGIN}", sapisid(&self.cookie)?).as_bytes(),
        );
        let authorization = format!("SAPISIDHASH {timestamp}_{digest:x}");
        let mut headers = HeaderMap::new();
        for (key, value) in [
            ("cookie", self.cookie.as_str()),
            ("x-goog-authuser", &self.authuser),
            ("authorization", &authorization),
            ("origin", MUSIC_ORIGIN),
            ("user-agent", MUSIC_USER_AGENT),
            ("accept", "*/*"),
        ] {
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| "Could not prepare the browser session")?;
            value.set_sensitive(matches!(key, "cookie" | "authorization"));
            headers.insert(key, value);
        }
        Ok(headers)
    }
}

#[cfg(windows)]
mod encryption {
    use std::{ffi::c_void, ptr};
    #[repr(C)]
    struct Blob {
        len: u32,
        data: *mut u8,
    }
    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            input: *const Blob,
            description: *const u16,
            entropy: *const Blob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            output: *mut Blob,
        ) -> i32;
        fn CryptUnprotectData(
            input: *const Blob,
            description: *mut *mut u16,
            entropy: *const Blob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            output: *mut Blob,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    pub fn replace_file(from: &std::path::Path, to: &std::path::Path) -> Result<(), String> {
        use std::os::windows::ffi::OsStrExt;
        let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 1 | 8) } == 0 {
            return Err("Could not finish saving browser credentials".into());
        }
        Ok(())
    }
    pub fn transform(bytes: &[u8], encrypt: bool) -> Result<Vec<u8>, String> {
        let input = Blob {
            len: bytes
                .len()
                .try_into()
                .map_err(|_| "Credential storage too large")?,
            data: bytes.as_ptr() as *mut u8,
        };
        let mut output = Blob {
            len: 0,
            data: ptr::null_mut(),
        };
        let ok = unsafe {
            if encrypt {
                CryptProtectData(
                    &input,
                    ptr::null(),
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    1,
                    &mut output,
                )
            } else {
                CryptUnprotectData(
                    &input,
                    ptr::null_mut(),
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    1,
                    &mut output,
                )
            }
        };
        if ok == 0 {
            return Err("Windows could not protect YouTube Music credentials".into());
        }
        let result =
            unsafe { std::slice::from_raw_parts(output.data, output.len as usize).to_vec() };
        unsafe {
            LocalFree(output.data.cast());
        }
        Ok(result)
    }
}

fn protect(bytes: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    {
        encryption::transform(bytes, true)
    }
    #[cfg(not(windows))]
    {
        Ok(bytes.to_vec())
    }
}

fn unprotect(bytes: &[u8]) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    {
        encryption::transform(bytes, false)
    }
    #[cfg(not(windows))]
    {
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn browser_fixture() -> BrowserCredentials {
        BrowserCredentials::parse(r#"{"Cookie":"SAPISID=fixture-cookie; SID=fixture-session","x-goog-authuser":"0","origin":"https://music.youtube.com","authorization":"Bearer must-never-be-used","x-custom":"ignored"}"#).unwrap()
    }

    #[test]
    fn browser_formats_are_data_only_and_authorization_is_recomputed() {
        let json = browser_fixture();
        let raw = BrowserCredentials::parse("POST /youtubei/v1/browse HTTP/2\nHost: music.youtube.com\nCookie: SAPISID=fixture-cookie; SID=fixture-session\nx-goog-authuser:\n0\nOrigin: https://music.youtube.com\nAuthorization: Bearer must-never-be-used").unwrap();
        let fetch = BrowserCredentials::parse(r#"await fetch("https://music.youtube.com/youtubei/v1/browse?key=ignored", {"headers":{"cookie":"SAPISID=fixture-cookie; SID=fixture-session","x-goog-authuser":"0","authorization":"must-never-be-used"},"body":"ignored","method":"POST"});"#).unwrap();
        for parsed in [json, raw, fetch] {
            let headers = parsed.headers(1_700_000_000).unwrap();
            assert_eq!(
                headers["cookie"],
                "SAPISID=fixture-cookie; SID=fixture-session"
            );
            assert_eq!(headers["origin"], MUSIC_ORIGIN);
            assert!(headers["authorization"]
                .to_str()
                .unwrap()
                .starts_with("SAPISIDHASH 1700000000_"));
            assert_eq!(
                headers["authorization"],
                "SAPISIDHASH 1700000000_ae4ac8f1cf49d804d280c29c1c58f422d9c7ad4d"
            );
            assert!(!headers["authorization"]
                .to_str()
                .unwrap()
                .contains("must-never"));
            assert!(!headers.contains_key("x-custom"));
            assert!(headers["authorization"].is_sensitive());
            assert!(headers["cookie"].is_sensitive());
            assert_ne!(
                headers["authorization"],
                parsed.headers(1_700_000_001).unwrap()["authorization"]
            );
        }
    }

    #[test]
    fn browser_parser_rejects_other_origins_scripts_missing_cookies_and_injection() {
        for input in [
            r#"fetch("https://evil.example/", {"headers":{"cookie":"SAPISID=fixture-cookie","x-goog-authuser":"0"}});"#,
            r#"fetch("https://music.youtube.com/", {"headers":{"cookie":"SAPISID=fixture-cookie","x-goog-authuser":"0"}}); doSomething()"#,
            r#"{"cookie":"SAPISID=fixture-cookie","x-goog-authuser":"0","origin":"https://music.youtube.com.evil.example"}"#,
            r#"{"cookie":"SID=fixture-session","x-goog-authuser":"0"}"#,
            r#"{"cookie":"SAPISID=fixture-cookie\r\nInjected: value","x-goog-authuser":"0"}"#,
            r#"{"cookie":"SAPISID=fixture-cookie","x-goog-authuser":"0\r\nInjected"}"#,
            r#"{"cookie":"SAPISID=one; SAPISID=two","x-goog-authuser":"0"}"#,
            r#"{"cookie":"SAPISID=fixture-cookie","x-goog-authuser":"0","origin":"http://music.youtube.com"}"#,
        ] {
            let error = BrowserCredentials::parse(input)
                .err()
                .expect("invalid export must fail");
            assert!(!error.contains("fixture-cookie"));
            assert!(!error.contains("fixture-session"));
        }
        assert!(BrowserCredentials::parse(&"x".repeat(MAX_BROWSER_BYTES as usize + 1)).is_err());
    }

    #[test]
    fn browser_signing_accepts_secure_cookie_without_sapisid() {
        let browser = BrowserCredentials::parse(
            r#"{"cookie":"__Secure-3PAPISID=fixture-cookie","x-goog-authuser":"1"}"#,
        )
        .unwrap();
        assert_eq!(
            browser.headers(1_700_000_000).unwrap()["authorization"],
            browser_fixture().headers(1_700_000_000).unwrap()["authorization"]
        );
    }

    #[test]
    fn browser_identity_and_oauth_identity_cannot_be_implicitly_mixed() {
        let directory =
            std::env::temp_dir().join(format!("sunder-browser-auth-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("oauth.bin");
        let browser_path = directory.join("browser-credentials.bin");
        let _ = std::fs::remove_file(&browser_path);
        let mut auth = Auth::new(path.clone());
        auth.credentials.client_id = "fixture.apps.googleusercontent.com".into();
        auth.credentials.refresh_token = "fixture-refresh".into();
        auth.credentials.namespace = "youtube-channel:UCaaaaaaaaaaaaaaaaaaaaaa".into();
        assert!(!auth.status().connected);
        assert!(auth.status().oauth_connected);
        assert_eq!(auth.status().auth_method, "oauth");
        assert!(auth.browser_namespace().is_err());
        let mut browser = browser_fixture();
        assert!(auth.save_browser(browser_fixture()).is_err());
        browser
            .set_verified_channel("UCbbbbbbbbbbbbbbbbbbbbbb".into())
            .unwrap();
        auth.save_browser(browser).unwrap();
        assert_eq!(
            auth.browser_namespace().unwrap(),
            "youtube-channel:UCbbbbbbbbbbbbbbbbbbbbbb"
        );
        assert_eq!(
            auth.status().identity_label.as_deref(),
            Some("UCbbbbbbbbbbbbbbbbbbbbbb")
        );
        let persisted = std::fs::read(&browser_path).unwrap();
        #[cfg(windows)]
        assert!(!persisted
            .windows(b"fixture-cookie".len())
            .any(|w| w == b"fixture-cookie"));
        assert!(
            serde_json::from_slice::<BrowserCredentials>(&unprotect(&persisted).unwrap())
                .unwrap()
                .is_verified()
        );
        let reloaded = Auth::new(path);
        assert_eq!(
            reloaded.browser_namespace().unwrap(),
            auth.browser_namespace().unwrap()
        );
        let prior = std::fs::read(&browser_path).unwrap();
        assert!(auth.save_browser(browser_fixture()).is_err());
        assert_eq!(std::fs::read(&browser_path).unwrap(), prior);
        let old_generation = auth.browser_generation;
        let mut replacement = browser_fixture();
        replacement
            .set_verified_channel("UCbbbbbbbbbbbbbbbbbbbbbb".into())
            .unwrap();
        auth.save_browser(replacement).unwrap();
        auth.invalidate_browser(old_generation).unwrap();
        assert!(
            auth.status().connected,
            "an old request cannot expire a replacement session for the same channel"
        );
        auth.invalidate_browser(auth.browser_generation).unwrap();
        assert!(!auth.status().connected);
        assert!(auth.status().configured);
        assert!(auth.browser_headers().is_err());
        let reloaded = Auth::new(directory.join("oauth.bin"));
        assert!(!reloaded.status().connected);
        assert_eq!(
            reloaded.status().identity_label.as_deref(),
            Some("UCbbbbbbbbbbbbbbbbbbbbbb")
        );
        let _ = std::fs::remove_file(browser_path);
        let _ = std::fs::remove_dir(directory);
    }

    #[test]
    fn browser_file_import_is_bounded_and_errors_never_echo_paths_or_contents() {
        let path = std::env::temp_dir().join(format!(
            "sunder-browser-size-test-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, vec![b'x'; MAX_BROWSER_BYTES as usize + 1]).unwrap();
        let error = BrowserCredentials::from_file(&path).err().unwrap();
        assert!(error.contains("128 KiB"));
        assert!(!error.contains(path.to_string_lossy().as_ref()));
        std::fs::remove_file(path).unwrap();
        assert!(BrowserCredentials::from_file(Path::new("relative.txt")).is_err());
    }

    #[test]
    fn oauth_failures_never_expose_server_payload() {
        let error = oauth_error(
            &json!({"error":"unknown","error_description":"secret fixture value","access_token":"secret fixture token"}),
        );
        assert!(!error.contains("secret"));
        assert!(!error.contains("token"));
    }

    #[test]
    fn immutable_channel_identity_is_stable_across_reconnects_and_separates_accounts() {
        let first =
            json!({"items":[{"id":"UCaaaaaaaaaaaaaaaaaaaaaa"}],"pageInfo":{"totalResults":1}});
        let reconnect = json!({"items":[{"id":"UCaaaaaaaaaaaaaaaaaaaaaa"}]});
        let other = json!({"items":[{"id":"UCbbbbbbbbbbbbbbbbbbbbbb"}]});
        assert_eq!(
            channel_namespace(&first).unwrap(),
            channel_namespace(&reconnect).unwrap()
        );
        assert_ne!(
            channel_namespace(&first).unwrap(),
            channel_namespace(&other).unwrap()
        );
        assert!(channel_namespace(&json!({"items":[]})).is_err());
        assert!(channel_namespace(
            &json!({"items":[{"id":"UCaaaaaaaaaaaaaaaaaaaaaa"},{"id":"UCbbbbbbbbbbbbbbbbbbbbbb"}]})
        )
        .is_err());
        assert!(channel_namespace(
            &json!({"items":[{"id":"UCaaaaaaaaaaaaaaaaaaaaaa"}],"nextPageToken":"more"})
        )
        .is_err());
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_round_trip_protects_credential_bytes() {
        let plain = br#"{"client_secret":"fixture-only-not-a-real-secret"}"#;
        let encrypted = protect(plain).unwrap();
        assert_ne!(encrypted.as_slice(), plain);
        assert!(!encrypted.windows(plain.len()).any(|bytes| bytes == plain));
        assert_eq!(unprotect(&encrypted).unwrap(), plain.to_vec());
        assert!(unprotect(b"not-a-protected-credential-file").is_err());
    }

    #[tokio::test]
    async fn pending_login_respects_server_poll_interval_without_network_request() {
        let mut auth =
            Auth::new(std::env::temp_dir().join("sunder-nonexistent-credential-fixture.bin"));
        auth.pending = Some(PendingLogin {
            device_code: "fixture".into(),
            expires_at: now() + 100,
            next_poll_at: now() + 30,
            interval: 30,
        });
        let result = auth.poll(&Client::new()).await.unwrap();
        assert_eq!(result.status, "pending");
        assert!(result.interval > 0);
        auth.pending.as_mut().unwrap().expires_at = now() - 1;
        assert!(auth.poll(&Client::new()).await.is_err());
        assert!(auth.pending.is_none());
    }
}
