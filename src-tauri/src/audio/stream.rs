//! Progressive HTTP decoding. ffmpeg only decodes to lossless float PCM;
//! Rodio owns the output device. No media file or growing RAM cache is created.
use std::io::{self, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::extraction::stream::StreamInfo;
use crate::process::NoWindow;
use rodio::Source;

const FRAMES_PER_CHUNK: usize = 2048;
const BUFFER_CHUNKS: usize = 48; // about two seconds at 48 kHz, <= 3 MiB at 8 channels

#[derive(Default)]
pub struct StreamClock {
    pub position_ms: AtomicU64,
    pub buffering: AtomicBool,
    pub ended: AtomicBool,
    pub failed: AtomicBool,
}

#[derive(Debug)]
enum Packet {
    Samples(Vec<f32>),
    End,
    Error(String),
}

#[derive(Clone)]
struct Session {
    current: Arc<AtomicUsize>,
    id: usize,
    cancel: Arc<AtomicBool>,
}
impl Session {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire) || self.current.load(Ordering::Acquire) != self.id
    }
}

fn send_packet(tx: &SyncSender<Packet>, mut packet: Packet, session: &Session) -> bool {
    loop {
        if session.cancelled() {
            return false;
        }
        match tx.try_send(packet) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(value)) => packet = value,
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

// Child's standard Drop does not kill/reap it. This guard covers errors/panics too.
struct DecoderChild(Child);
impl Drop for DecoderChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn read_pcm(
    mut reader: impl Read,
    tx: &SyncSender<Packet>,
    session: &Session,
    channels: u16,
) -> io::Result<()> {
    let mut bytes = vec![0; FRAMES_PER_CHUNK * usize::from(channels) * 4];
    loop {
        let mut filled = 0;
        while filled < bytes.len() {
            if session.cancelled() {
                return Ok(());
            }
            match reader.read(&mut bytes[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if filled == 0 {
            return Ok(());
        }
        if filled % (usize::from(channels) * 4) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated PCM frame",
            ));
        }
        let samples = bytes[..filled]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        if !send_packet(tx, Packet::Samples(samples), session) {
            return Ok(());
        }
        if filled < bytes.len() {
            return Ok(());
        }
    }
}

struct Pipeline {
    rx: Receiver<Packet>,
    session: Session,
}
impl Drop for Pipeline {
    fn drop(&mut self) {
        self.session.cancel.store(true, Ordering::Release);
    }
}

impl Pipeline {
    fn launch(
        bin: String,
        info: Arc<StreamInfo>,
        current: Arc<AtomicUsize>,
        id: usize,
        rate: u32,
        channels: u16,
        position: Duration,
    ) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(BUFFER_CHUNKS);
        let session = Session {
            current,
            id,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let worker_session = session.clone();
        std::thread::Builder::new()
            .name(format!("sunder-stream-{id}"))
            .spawn(move || {
                if worker_session.cancelled() {
                    return;
                }
                let mut command = Command::new(bin);
                command.no_window().args([
                    "-nostdin",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-rw_timeout",
                    "15000000",
                    "-reconnect",
                    "1",
                    "-reconnect_streamed",
                    "1",
                    "-reconnect_delay_max",
                    "3",
                    "-reconnect_max_retries",
                    "3",
                    "-reconnect_delay_total_max",
                    "15",
                    "-protocol_whitelist",
                    "http,https,tcp,tls,crypto",
                ]);
                if !info.headers.is_empty() {
                    command.args(["-headers", &info.headers]);
                }
                if !position.is_zero() {
                    command.args(["-ss", &format!("{:.6}", position.as_secs_f64())]);
                }
                command
                    .args([
                        "-i",
                        &info.url,
                        "-map",
                        "0:a:0",
                        "-vn",
                        "-sn",
                        "-dn",
                        "-acodec",
                        "pcm_f32le",
                        "-f",
                        "f32le",
                        "-ar",
                        &rate.to_string(),
                        "-ac",
                        &channels.to_string(),
                        "pipe:1",
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null());
                let mut child = match command.spawn() {
                    Ok(child) => DecoderChild(child),
                    Err(e) => {
                        send_packet(
                            &tx,
                            Packet::Error(format!("failed to start ffmpeg: {e}")),
                            &worker_session,
                        );
                        return;
                    }
                };
                let stdout = child.0.stdout.take().expect("piped stdout");
                let reader_tx = tx.clone();
                let reader_session = worker_session.clone();
                let (read_done_tx, read_done_rx) = mpsc::sync_channel(1);
                let reader = std::thread::Builder::new()
                    .name(format!("sunder-pcm-{id}"))
                    .spawn(move || {
                        let result = read_pcm(stdout, &reader_tx, &reader_session, channels);
                        let _ = read_done_tx.send(result);
                    });
                let Ok(reader) = reader else {
                    send_packet(
                        &tx,
                        Packet::Error("failed to start PCM reader".into()),
                        &worker_session,
                    );
                    return;
                };
                let mut read_result = None;
                let exit_result = loop {
                    if worker_session.cancelled() {
                        break None;
                    }
                    if let Ok(result) = read_done_rx.try_recv() {
                        if result.is_err() {
                            read_result = Some(result);
                            break None;
                        }
                        read_result = Some(result);
                    }
                    match child.0.try_wait() {
                        Ok(Some(status)) => break Some(status.success()),
                        Err(_) => break Some(false),
                        Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                    }
                };
                // Killing the process also unblocks an outstanding pipe read.
                if exit_result.is_none() {
                    let _ = child.0.kill();
                }
                let _ = child.0.wait();
                // At normal EOF drain queued PCM with backpressure before the terminal packet.
                let _ = reader.join();
                let read_ok = read_result
                    .or_else(|| read_done_rx.try_recv().ok())
                    .is_some_and(|r| r.is_ok());
                if !worker_session.cancelled() {
                    let packet = if exit_result == Some(true) && read_ok {
                        Packet::End
                    } else {
                        Packet::Error(
                            "audio stream interrupted or decoder failed; retry playback".into(),
                        )
                    };
                    send_packet(&tx, packet, &worker_session);
                }
            })?;
        Ok(Self { rx, session })
    }
}

pub struct HttpSource {
    pipeline: Pipeline,
    bin: String,
    info: Arc<StreamInfo>,
    rate: u32,
    channels: u16,
    samples: std::vec::IntoIter<f32>,
    consumed: u64,
    base_ms: u64,
    clock: Arc<StreamClock>,
    on_error: Arc<dyn Fn(String) + Send + Sync>,
    ended: bool,
    silence_remaining: u16,
}

impl HttpSource {
    pub fn new(
        bin: String,
        info: StreamInfo,
        current: Arc<AtomicUsize>,
        id: usize,
        position: Duration,
        on_error: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Result<(Self, Arc<StreamClock>), String> {
        let rate = info.sample_rate_hz.unwrap_or(48_000).max(1);
        let channels = info.channels.unwrap_or(2).max(1);
        let info = Arc::new(info);
        let pipeline = Pipeline::launch(
            bin.clone(),
            info.clone(),
            current,
            id,
            rate,
            channels,
            position,
        )
        .map_err(|e| format!("failed to start streaming worker: {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(30);
        let samples = loop {
            if pipeline.session.cancelled() {
                return Err("session superseded".into());
            }
            if Instant::now() >= deadline {
                return Err("audio stream startup timed out".into());
            }
            match pipeline.rx.recv_timeout(Duration::from_millis(20)) {
                Ok(Packet::Samples(samples)) => break samples.into_iter(),
                Ok(Packet::Error(e)) => return Err(e),
                Ok(Packet::End) => return Err("audio stream contains no samples".into()),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("audio decoder stopped".into())
                }
                Err(mpsc::RecvTimeoutError::Timeout) => (),
            }
        };
        let clock = Arc::new(StreamClock::default());
        clock
            .position_ms
            .store(position.as_millis() as u64, Ordering::Release);
        Ok((
            Self {
                pipeline,
                bin,
                info,
                rate,
                channels,
                samples,
                consumed: 0,
                base_ms: position.as_millis() as u64,
                clock: clock.clone(),
                on_error,
                ended: false,
                silence_remaining: 0,
            },
            clock,
        ))
    }

    fn update_position(&self) {
        self.clock.position_ms.store(
            self.base_ms + self.consumed * 1000 / u64::from(self.channels) / u64::from(self.rate),
            Ordering::Release,
        );
    }
}

impl Iterator for HttpSource {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if self.ended || self.pipeline.session.cancelled() {
            return None;
        }
        if self.silence_remaining > 0 {
            self.silence_remaining -= 1;
            return Some(0.0);
        }
        if let Some(sample) = self.samples.next() {
            self.consumed += 1;
            return Some(sample);
        }
        self.update_position();
        match self.pipeline.rx.try_recv() {
            Ok(Packet::Samples(samples)) => {
                self.clock.buffering.store(false, Ordering::Release);
                self.samples = samples.into_iter();
                self.next()
            }
            Err(TryRecvError::Empty) => {
                // Never block the audio callback on HTTP. Silence does not advance the media clock.
                self.clock.buffering.store(true, Ordering::Release);
                self.silence_remaining = self.channels - 1;
                Some(0.0)
            }
            packet => {
                if let Ok(Packet::Error(error)) = packet {
                    self.clock.failed.store(true, Ordering::Release);
                    (self.on_error)(error);
                }
                self.ended = true;
                self.clock.ended.store(true, Ordering::Release);
                None
            }
        }
    }
}

impl Source for HttpSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> rodio::ChannelCount {
        rodio::ChannelCount::new(self.channels).unwrap()
    }
    fn sample_rate(&self) -> rodio::SampleRate {
        rodio::SampleRate::new(self.rate).unwrap()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.info.duration
    }
    fn try_seek(&mut self, position: Duration) -> Result<(), rodio::source::SeekError> {
        let position = self
            .info
            .duration
            .map_or(position, |duration| position.min(duration));
        let pipeline = Pipeline::launch(
            self.bin.clone(),
            self.info.clone(),
            self.pipeline.session.current.clone(),
            self.pipeline.session.id,
            self.rate,
            self.channels,
            position,
        )
        .map_err(|e| rodio::source::SeekError::Other(Arc::new(e)))?;
        // Dropping the old pipeline cancels its decoder, even if paused or its buffer is full.
        self.pipeline = pipeline;
        self.samples = Vec::new().into_iter();
        self.consumed = 0;
        self.base_ms = position.as_millis() as u64;
        self.ended = false;
        self.silence_remaining = 0;
        self.clock.failed.store(false, Ordering::Release);
        self.clock.ended.store(false, Ordering::Release);
        self.clock.buffering.store(true, Ordering::Release);
        self.update_position();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session() -> Session {
        Session {
            current: Arc::new(AtomicUsize::new(1)),
            id: 1,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn a_full_buffer_applies_backpressure_and_cancellation_releases_producer() {
        let (tx, rx) = mpsc::sync_channel(2);
        let session = session();
        let producer_session = session.clone();
        let producer = std::thread::spawn(move || {
            for _ in 0..1000 {
                if !send_packet(&tx, Packet::Samples(vec![1.0; 16]), &producer_session) {
                    return;
                }
            }
            panic!("producer must stop at the bounded buffer");
        });
        std::thread::sleep(Duration::from_millis(30));
        assert!(!producer.is_finished());
        session.cancel.store(true, Ordering::Release);
        producer.join().unwrap();
        assert_eq!(rx.try_iter().count(), 2);
    }

    #[test]
    fn partial_reads_preserve_pcm_frame_alignment_and_last_samples() {
        struct SmallReads(std::io::Cursor<Vec<u8>>);
        impl Read for SmallReads {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                let size = bytes.len().min(3);
                self.0.read(&mut bytes[..size])
            }
        }
        let data: Vec<_> = [0.25f32, -0.75, 0.5, -1.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        let (tx, rx) = mpsc::sync_channel(2);
        read_pcm(SmallReads(io::Cursor::new(data)), &tx, &session(), 2).unwrap();
        match rx.recv().unwrap() {
            Packet::Samples(samples) => assert_eq!(samples, [0.25, -0.75, 0.5, -1.0]),
            _ => panic!("expected PCM"),
        }
        assert!(read_pcm(io::Cursor::new(vec![0; 7]), &tx, &session(), 2).is_err());
    }

    #[test]
    fn changing_track_cancels_a_producer_with_a_full_buffer() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let session = session();
        assert!(send_packet(&tx, Packet::Samples(vec![0.0]), &session));
        session.current.store(2, Ordering::Release);
        assert!(!send_packet(&tx, Packet::End, &session));
    }

    #[test]
    fn starvation_is_nonblocking_freezes_media_time_and_preserves_stereo_alignment() {
        let (tx, rx) = mpsc::sync_channel(2);
        let session = session();
        let clock = Arc::new(StreamClock::default());
        let info = StreamInfo {
            url: "http://unused.invalid/".into(),
            headers: String::new(),
            codec: None,
            bitrate_kbps: None,
            sample_rate_hz: Some(1000),
            channels: Some(2),
            format_id: None,
            duration: None,
        };
        let mut source = HttpSource {
            pipeline: Pipeline { rx, session },
            bin: String::new(),
            info: Arc::new(info),
            rate: 1000,
            channels: 2,
            samples: vec![0.25, -0.25].into_iter(),
            consumed: 0,
            base_ms: 0,
            clock: clock.clone(),
            on_error: Arc::new(|_| panic!("unexpected error")),
            ended: false,
            silence_remaining: 0,
        };
        assert_eq!(source.next(), Some(0.25));
        assert_eq!(source.next(), Some(-0.25));
        assert_eq!(source.next(), Some(0.0)); // left channel of one silent frame
        tx.send(Packet::Samples(vec![-1.0, 1.0])).unwrap();
        assert_eq!(source.next(), Some(0.0)); // finish silence before accepting a new stereo frame
        assert_eq!(source.next(), Some(-1.0));
        assert_eq!(source.next(), Some(1.0));
        let start = Instant::now();
        for _ in 0..1000 {
            assert_eq!(source.next(), Some(0.0));
        }
        assert!(start.elapsed() < Duration::from_millis(100));
        assert_eq!(
            clock.position_ms.load(Ordering::Acquire),
            2,
            "silence must not count as media time"
        );
        assert!(clock.buffering.load(Ordering::Acquire));
        tx.send(Packet::End).unwrap();
        assert!(source.next().is_none());
        assert!(clock.ended.load(Ordering::Acquire));
    }

    /// No sound device and no Internet: exercise the real decoder against a
    /// throttled HTTP fixture. Opt in with SUNDER_TEST_FFMPEG=<ffmpeg path>.
    #[test]
    fn real_http_decoder_starts_before_eof_and_seeks_without_downloading_a_file() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let Ok(bin) = std::env::var("SUNDER_TEST_FFMPEG") else {
            return;
        };
        let rate = 48_000u32;
        let frames = rate * 20;
        let bytes_len = frames * 2;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + bytes_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&bytes_len.to_le_bytes());
        // Distinct halves make a successful seek observable in decoded samples.
        for frame in 0..frames {
            wav.extend_from_slice(
                &(if frame < rate * 5 { 8192i16 } else { -8192i16 }).to_le_bytes(),
            );
        }
        let wav = Arc::new(wav);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicUsize::new(0));
        let connections = Arc::new(AtomicUsize::new(0));
        let server_stop = stop.clone();
        let server_complete = completed.clone();
        let server_connections = connections.clone();
        let server = std::thread::spawn(move || {
            let mut clients = Vec::new();
            while !server_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        server_connections.fetch_add(1, Ordering::Release);
                        let wav = wav.clone();
                        let stop = server_stop.clone();
                        let completed = server_complete.clone();
                        clients.push(std::thread::spawn(move || {
                            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                            socket.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
                            let mut request = BufReader::new(socket.try_clone().unwrap());
                            let mut line = String::new(); let mut offset = 0usize;
                            loop {
                                line.clear();
                                if request.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" { break; }
                                if let Some(range) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                                    offset = range.split('-').next().unwrap().trim().parse().unwrap_or(0);
                                }
                            }
                            offset = offset.min(wav.len());
                            let header = format!("HTTP/1.1 206 Partial Content\r\nContent-Type: audio/wav\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n", wav.len() - offset, offset, wav.len() - 1, wav.len());
                            if socket.write_all(header.as_bytes()).is_err() { return; }
                            for chunk in wav[offset..].chunks(4096) {
                                if stop.load(Ordering::Acquire) || socket.write_all(chunk).is_err() { return; }
                                std::thread::sleep(Duration::from_millis(4));
                            }
                            completed.fetch_add(1, Ordering::Release);
                        }));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("fixture accept: {e}"),
                }
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        struct StopFixture(Arc<AtomicBool>, Option<std::thread::JoinHandle<()>>);
        impl Drop for StopFixture {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
                if let Some(server) = self.1.take() {
                    server.join().unwrap();
                }
            }
        }
        let _fixture = StopFixture(stop, Some(server));
        let info = StreamInfo {
            url: format!("http://{address}/test.wav"),
            headers: String::new(),
            codec: Some("pcm_s16le".into()),
            bitrate_kbps: Some(768.0),
            sample_rate_hz: Some(rate),
            channels: Some(1),
            format_id: None,
            duration: Some(Duration::from_secs(20)),
        };
        let session = Arc::new(AtomicUsize::new(1));
        let (mut source, clock) = HttpSource::new(
            bin,
            info,
            session.clone(),
            1,
            Duration::ZERO,
            Arc::new(|error| panic!("decoder error: {error}")),
        )
        .unwrap();
        assert_eq!(
            completed.load(Ordering::Acquire),
            0,
            "playback must start before HTTP EOF"
        );
        assert!((source.next().unwrap() - 0.25).abs() < 0.001);
        let old_cancel = source.pipeline.session.cancel.clone();
        source.try_seek(Duration::from_secs(10)).unwrap();
        assert!(old_cancel.load(Ordering::Acquire));
        assert_eq!(clock.position_ms.load(Ordering::Acquire), 10_000);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut found = false;
        while Instant::now() < deadline {
            if source
                .next()
                .is_some_and(|sample| (sample + 0.25).abs() < 0.001)
            {
                found = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            found,
            "seek must replace old buffered samples with audio at 10s"
        );
        assert!(
            connections.load(Ordering::Acquire) >= 2,
            "seek should open a fresh HTTP stream"
        );
        session.store(2, Ordering::Release);
        assert!(
            source.next().is_none(),
            "next track must cancel the old source"
        );
        drop(source);
    }
}
