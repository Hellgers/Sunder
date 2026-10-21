//! Resolve one audio format, never download or transcode a media file.
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::process::NoWindow;

// Deliberately not Debug/Serialize: signed URLs and HTTP headers stay private.
pub struct StreamInfo {
    pub url: String,
    pub headers: String,
    pub codec: Option<String>,
    pub bitrate_kbps: Option<f64>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u16>,
    pub format_id: Option<String>,
    pub duration: Option<Duration>,
}

impl StreamInfo {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|_| "yt-dlp returned invalid stream metadata".to_string())?;
        let url = value["url"].as_str().filter(|url| {
            url.starts_with("https://") || url.starts_with("http://")
        }).ok_or("yt-dlp returned no HTTP audio stream")?.to_string();
        let mut headers = String::new();
        if let Some(values) = value["http_headers"].as_object() {
            for (name, value) in values {
                if let Some(value) = value.as_str() {
                    // Never turn metadata into extra HTTP headers.
                    if !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        && !value.contains('\r') && !value.contains('\n')
                    {
                        headers.push_str(&format!("{name}: {value}\r\n"));
                    }
                }
            }
        }
        let positive = |key: &str| value[key].as_f64().filter(|n| n.is_finite() && *n > 0.0);
        Ok(Self {
            url, headers,
            codec: value["acodec"].as_str().filter(|s| *s != "none").map(str::to_string),
            bitrate_kbps: positive("abr"),
            sample_rate_hz: positive("asr").filter(|n| *n <= 384_000.0).map(|n| n as u32),
            channels: positive("audio_channels").filter(|n| *n <= 8.0).map(|n| n as u16),
            format_id: value["format_id"].as_str().map(str::to_string),
            duration: positive("duration").and_then(|n| Duration::try_from_secs_f64(n).ok()),
        })
    }
}

/// Keep diagnostic categories without returning URLs, cookies or raw stderr.
fn safe_error(stderr: &[u8]) -> String {
    let lower = String::from_utf8_lossy(stderr).to_lowercase();
    if lower.contains("sign in to confirm") && lower.contains("not a bot") {
        "Sign in to confirm you're not a bot (choose a session cookie source)".into()
    } else if lower.contains("403") {
        "HTTP Error 403: stream access denied; update yt-dlp or retry".into()
    } else if lower.contains("private video") || lower.contains("video unavailable") {
        "This video is unavailable".into()
    } else {
        "yt-dlp could not resolve an available audio stream".into()
    }
}

pub fn resolve(
    bin: &str, video_id: &str, session: &Arc<AtomicUsize>, session_id: usize,
) -> Result<StreamInfo, String> {
    if video_id.is_empty() || !video_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("invalid video ID".into());
    }
    let mut child = Command::new(bin).no_window()
        .args(["--ignore-config", "--dump-single-json", "--skip-download", "--no-playlist",
            "--no-warnings", "--socket-timeout", "15", "--retries", "2", "-f", "bestaudio/best"])
        .args(super::cookies::current_args())
        .arg(format!("https://www.youtube.com/watch?v={video_id}"))
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().map_err(|e| format!("failed to start yt-dlp: {e}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (out_tx, out_rx) = mpsc::sync_channel(1);
    let out_reader = std::thread::spawn(move || {
        let mut data = Vec::new();
        let result = stdout.take(8 * 1024 * 1024 + 1).read_to_end(&mut data);
        let _ = out_tx.send(if result.is_ok() && data.len() <= 8 * 1024 * 1024 {
            Ok(data)
        } else { Err("yt-dlp stream metadata exceeded its size limit".to_string()) });
    });
    let err_reader = std::thread::spawn(move || {
        let mut data = Vec::new();
        // Drain fully to avoid a full stderr pipe; retain only a bounded prefix.
        let mut reader = stderr;
        let mut chunk = [0; 4096];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 { break; }
            let keep = n.min((64usize * 1024).saturating_sub(data.len()));
            data.extend_from_slice(&chunk[..keep]);
        }
        data
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut data = None;
    let result = loop {
        if session.load(Ordering::SeqCst) != session_id {
            break Err("session superseded".into());
        }
        if Instant::now() >= deadline { break Err("yt-dlp stream resolution timed out".into()); }
        if let Ok(output) = out_rx.try_recv() {
            match output { Ok(output) => data = Some(output), Err(e) => break Err(e) }
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status.success()),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break Err("failed to wait for yt-dlp".into()),
        }
    };
    if result.is_err() { let _ = child.kill(); }
    let _ = child.wait();
    let _ = out_reader.join();
    let stderr = err_reader.join().unwrap_or_default();
    if !result? { return Err(safe_error(&stderr)); }
    let data = data.or_else(|| out_rx.try_recv().ok().and_then(Result::ok))
        .ok_or("yt-dlp returned no stream metadata")?;
    StreamInfo::parse(&data)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_keeps_actual_quality_and_filters_header_injection() {
        let info = StreamInfo::parse(br#"{"url":"https://media.invalid/audio?secret=1","acodec":"opus","abr":129.3,"asr":48000,"audio_channels":2,"format_id":"251","http_headers":{"User-Agent":"test","Injected":"bad\r\nCookie: secret"}}"#).unwrap();
        assert_eq!(info.codec.as_deref(), Some("opus"));
        assert_eq!(info.bitrate_kbps, Some(129.3));
        assert_eq!(info.headers, "User-Agent: test\r\n");
        assert!(info.duration.is_none());
    }
    #[test]
    fn errors_never_return_signed_urls_or_cookie_values() {
        let error = safe_error(b"ERROR HTTP Error 403 https://media.invalid/?sig=secret Cookie: secret");
        assert!(error.contains("403"));
        assert!(!error.contains("secret"));
        assert!(!error.contains("https://"));
    }
}
