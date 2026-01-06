use anyhow::Result;
use futures_util::{StreamExt, SinkExt};
use librespot_core::config::SessionConfig;
use librespot_core::session::Session;
use librespot_playback::audio_backend::{Sink, SinkError};
use librespot_playback::config::PlayerConfig;
use librespot_playback::mixer::{Mixer, MixerConfig};
use librespot_playback::decoder::AudioPacket;
use librespot_connect::spirc::Spirc;
use librespot_playback::player::{Player, PlayerEvent};
use log::{error, info, warn};
use mumble_protocol::control::{ClientControlCodec, ControlPacket};
use mumble_protocol::crypt::ClientCryptState;
use mumble_protocol::voice::{Serverbound, VoicePacket, VoicePacketPayload};
use opus::{Application, Channels, Encoder};
use ringbuf::{HeapRb, Producer};
use rubato::{FftFixedIn, Resampler};
use std::convert::TryInto;
use std::net::{Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time;
use tokio::net::UdpSocket;
use tokio_native_tls::native_tls::TlsConnector;
use tokio_util::codec::Decoder;
use tokio_util::udp::UdpFramed;
use librespot_playback::mixer::VolumeGetter;
use librespot_discovery::Discovery;

// Audio Constants
const MUMBLE_SAMPLE_RATE: usize = 48000;
// Mumble commonly uses 20ms Opus frames; smaller frames reduce audible impact of loss/stalls.
const MUMBLE_FRAME_DURATION_MS: u64 = 10;
const MUMBLE_FRAME_SIZE: usize = (MUMBLE_SAMPLE_RATE as u64 * MUMBLE_FRAME_DURATION_MS/ 1000) as usize;
// const SPOTIFY_FRAME_SIZE: usize = 882;
const SPOTIFY_FRAME_SIZE: usize = (882.0 * (MUMBLE_FRAME_DURATION_MS as f64 / 20.0)) as usize;

const CHANNELS: usize = 2;
// Keep the ring buffer small so librespot can't decode far ahead (which would add lag).
// This also provides backpressure to keep playback near real-time.
const RINGBUF_SIZE: usize = SPOTIFY_FRAME_SIZE * CHANNELS * 8;

#[derive(Clone)]
struct SharedMixer {
    inner: Arc<Mutex<librespot_playback::mixer::softmixer::SoftMixer>>,
}

impl VolumeGetter for SharedMixer {
    fn attenuation_factor(&self) -> f64 {
        let vol = self.inner.lock().unwrap().volume();
        (vol as f64 / 65535.0).powi(3)
    }
}

impl Mixer for SharedMixer {
    fn open(_config: MixerConfig) -> Self {
        let soft_mixer = librespot_playback::mixer::softmixer::SoftMixer::open(_config);
        Self { inner: Arc::new(Mutex::new(soft_mixer)) }
    }
    fn set_volume(&self, volume: u16) {
        self.inner.lock().unwrap().set_volume(volume);
    }
    fn volume(&self) -> u16 {
        self.inner.lock().unwrap().volume()
    }
}

// --- Spotify Audio Backend ---

struct RingbufSink {
    producer: Producer<f32, Arc<HeapRb<f32>>>,
    scratch: Vec<f32>,
}

impl Sink for RingbufSink {
    fn start(&mut self) -> Result<(), SinkError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SinkError> {
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, _converter: &mut librespot_playback::convert::Converter) -> Result<(), SinkError> {
        match packet {
            AudioPacket::Samples(samples) => {
                self.scratch.resize(samples.len(), 0.0);
                for (i, &s) in samples.iter().enumerate() {
                    // librespot provides normalized PCM samples as f64 in roughly [-1.0, 1.0].
                    // Opus expects f32 PCM in the same range.
                    self.scratch[i] = s as f32;
                }

                let mut written = 0;
                while written < self.scratch.len() {
                    let n = self.producer.push_slice(&self.scratch[written..]);
                    if n == 0 {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    written += n;
                }
            }
            AudioPacket::OggData(_) => {
                warn!("Received OggData packet, ignoring.");
            }
        }
        Ok(())
    }
}

// --- Main ---

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    info!("Starting MumbleDJ...");

    // 1. Setup RingBuffer
    let rb = HeapRb::<f32>::new(RINGBUF_SIZE);
    let (producer, mut consumer) = rb.split();

    // 2. Setup Channels
    enum SpotifyCommand {
        Stop,
        Volume(u16),
    }
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SpotifyCommand>();

    // 3. Spawn Spotify Thread
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        
        rt.block_on(async move {
            info!("Spotify thread started.");
            
            let cache = librespot_core::cache::Cache::new(Some(std::path::PathBuf::from("./credentials")), None, None, None).ok();
            let config = SessionConfig::default();
            let device_id = config.device_id.clone();

            let credentials = if let Some(cache) = &cache {
                if let Some(creds) = cache.credentials() {
                    info!("Using cached credentials.");
                    creds
                } else {
                    info!("No cached credentials found. Starting Zeroconf discovery...");
                    info!("Please open Spotify on a device on the same network and connect to 'MumbleDJ'.");
                    let mut discovery = Discovery::builder(device_id)
                        .name("MumbleDJ")
                        .launch()
                        .expect("Failed to create discovery server");
                    
                    discovery.next().await.expect("Discovery failed")
                }
            } else {
                 info!("Cache not available. Starting Zeroconf discovery...");
                 let mut discovery = Discovery::builder(device_id)
                        .name("MumbleDJ")
                        .launch()
                        .expect("Failed to create discovery server");
                    
                 discovery.next().await.expect("Discovery failed")
            };

            info!("Credentials obtained. Connecting session...");
            let session = Session::connect(
                config, 
                credentials,
                cache, 
                true
            ).await;

            match session {
                Ok(session) => {
                    info!("Spotify Connected!");
                    
                    let mixer_config = MixerConfig::default();
                    let player_config = PlayerConfig::default();

                    let sink = Box::new(RingbufSink { producer, scratch: Vec::new() });
                    let backend_sink = Arc::new(Mutex::new(Some(sink)));
                    
                    let backend_fn = move || {
                        let s = backend_sink.lock().unwrap().take().expect("Sink already taken");
                        s as Box<dyn Sink>
                    };

                    let (session_obj, _creds) = session;

                    let soft_mixer = librespot_playback::mixer::softmixer::SoftMixer::open(mixer_config);
                    let shared_mixer = SharedMixer { inner: Arc::new(Mutex::new(soft_mixer)) };

                    let (player, mut event_channel) = Player::new(
                        player_config,
                        session_obj.clone(),
                        // Use SharedMixer for actual audio attenuation so Spotify volume changes affect output.
                        Box::new(shared_mixer.clone()),
                        backend_fn
                    );

                    let (_spirc, spirc_task) = Spirc::new(
                        Default::default(),
                        session_obj,
                        player,
                        Box::new(shared_mixer.clone())
                    );
                    
                    tokio::spawn(spirc_task);

                    let shared_mixer_events = shared_mixer.clone();
                    tokio::spawn(async move {
                        while let Some(event) = event_channel.recv().await {
                            match event {
                                PlayerEvent::VolumeSet { volume } => {
                                    // librespot volume is 0..=65535
                                    shared_mixer_events.set_volume(volume);
                                    info!("Spotify VolumeSet -> applied volume {}", volume);
                                }
                                _ => info!("Spotify Event: {:?}", event),
                            }
                        }
                    });

                    // Command Loop
                    while let Some(cmd) = cmd_rx.recv().await {
                        match cmd {
                            SpotifyCommand::Stop => info!("Local !stop ignored."),
                            SpotifyCommand::Volume(v) => {
                                let vol = v.min(100) as f64 / 100.0;
                                let val = (vol.powf(3.0) * 65535.0) as u16;
                                shared_mixer.set_volume(val);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to connect to Spotify: {:?}", e);
                }
            }
        });
    });

    // 4. Connect to Mumble
    let mumble_server = "localhost:64738";
    let mumble_user = "mumtify";

    let server_addr: SocketAddr = tokio::net::lookup_host(mumble_server)
        .await?
        .next()
        .ok_or_else(|| anyhow::anyhow!("Failed to resolve Mumble server address"))?;
    let server_host = mumble_server
        .split(':')
        .next()
        .unwrap_or(mumble_server)
        .to_string();

    // UDP voice setup: control channel provides CryptSetup (key + nonces)
    let (crypt_state_tx, mut crypt_state_rx) = mpsc::channel::<ClientCryptState>(4);
    let mut pending_crypt_state: Option<ClientCryptState> = None;
    let mut control_synced = false;

    // Channel for sending encoded voice packets to the UDP task
    // Small queue to tolerate minor jitter without building up lag.
    let (udp_voice_tx, mut udp_voice_rx) = mpsc::channel::<VoicePacket<Serverbound>>(10);

    // Spawn UDP task that owns the UDP socket + crypto state and sends audio packets
    tokio::spawn(async move {
        let udp_socket = match UdpSocket::bind((Ipv6Addr::from(0u128), 0u16)).await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to bind UDP socket: {e}");
                return;
            }
        };

        let Some(crypt_state) = crypt_state_rx.recv().await else {
            warn!("UDP crypto state never received; UDP audio disabled");
            return;
        };

        info!("UDP crypto ready; switching voice to UDP");

        let mut framed = UdpFramed::new(udp_socket, crypt_state);

        // Prime the server/NAT mapping with a dummy voice packet (pattern used by mumble-protocol example)
        let prime = VoicePacket::Audio {
            _dst: std::marker::PhantomData,
            target: 0,
            session_id: (),
            seq_num: 0,
            payload: VoicePacketPayload::Opus([0u8; 128].as_ref().to_vec().into(), true),
            position_info: None,
        };
        let _ = framed.send((prime, server_addr)).await;

        let mut udp_ping_interval = time::interval(Duration::from_secs(5));
        udp_ping_interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                Some(packet) = udp_voice_rx.recv() => {
                    if framed.send((packet, server_addr)).await.is_err() {
                        warn!("UDP sink closed; stopping UDP audio");
                        break;
                    }
                }

                Some(new_state) = crypt_state_rx.recv() => {
                    // Server requested crypto resync (e.g. after packet loss). Update state in-place.
                    *framed.codec_mut() = new_state;
                    info!("UDP crypto state updated (CryptSetup resync)");
                }

                _ = udp_ping_interval.tick() => {
                    // Keep UDP path alive; many servers use this for UDP latency stats.
                    // We don't currently track timestamps; server will still see traffic.
                    let ping: VoicePacket<Serverbound> = VoicePacket::Ping { timestamp: 0 };
                    let _ = framed.send((ping, server_addr)).await;
                }
            }
        }
    });

    info!("Connecting to Mumble server: {}", mumble_server);
    let socket = tokio::net::TcpStream::connect(mumble_server).await?;
    let cx = TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .build()?;
    let tokio_cx = tokio_native_tls::TlsConnector::from(cx);
    let tls_stream = tokio_cx.connect(&server_host, socket).await?;

    let (mut control_tx, mut control_rx) = ClientControlCodec::new().framed(tls_stream).split();

    let mut version = mumble_protocol::control::msgs::Version::new();
    version.set_version(1 << 16 | 3 << 8 | 0);
    version.set_release("Mumtify 0.1".into());
    version.set_os("Rust".into());
    control_tx.send(ControlPacket::Version(Box::new(version))).await?;

    let mut authenticate = mumble_protocol::control::msgs::Authenticate::new();
    authenticate.set_username(mumble_user.into());
    authenticate.set_opus(true);
    control_tx.send(ControlPacket::Authenticate(Box::new(authenticate))).await?;

    let mut _session_id = None;
    
    // 5. Audio Processing Loop
    let udp_voice_tx_audio = udp_voice_tx.clone();
    tokio::spawn(async move {
        let mut encoder = Encoder::new(MUMBLE_SAMPLE_RATE as u32, Channels::Stereo, Application::Audio).unwrap();
        encoder.set_bitrate(opus::Bitrate::Bits(96000)).unwrap();
        let _ = encoder.set_inband_fec(true);
        let _ = encoder.set_packet_loss_perc(10);
        let _ = encoder.set_complexity(5);

        let mut resampler = FftFixedIn::<f32>::new(SPOTIFY_FRAME_SIZE, MUMBLE_FRAME_SIZE, SPOTIFY_FRAME_SIZE, 1, CHANNELS).unwrap();
        
        let mut input_buffer_interleaved = vec![0.0; SPOTIFY_FRAME_SIZE * CHANNELS];
        let mut input_buffer_planar = vec![vec![0.0; SPOTIFY_FRAME_SIZE]; CHANNELS];
        let mut output_buffer_planar = vec![vec![0.0; MUMBLE_FRAME_SIZE]; CHANNELS];
        let mut output_buffer_interleaved = vec![0.0; MUMBLE_FRAME_SIZE * CHANNELS];
        let silence_pcm = vec![0.0f32; MUMBLE_FRAME_SIZE * CHANNELS];

        let frame_in_samples = SPOTIFY_FRAME_SIZE * CHANNELS;
        let target_buffer_frames: usize = 4; // ~80ms with 20ms frames
        let target_buffer_samples = target_buffer_frames * frame_in_samples;
        
        let mut seq_num = 0u64;
        let mut ticker = time::interval(Duration::from_millis(MUMBLE_FRAME_DURATION_MS));
        // Avoid "catch-up" bursts if the task is delayed (bursts can create jitter/lag).
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        let mut underflow_frames: u64 = 0;
        let mut last_underflow_log = time::Instant::now();
        let mut consecutive_underflows: u64 = 0;
        let mut stream_active = false;

        // Wait for a tiny prebuffer to smooth startup (but avoid large delay).
        while consumer.len() < target_buffer_samples {
            time::sleep(Duration::from_millis(5)).await;
        }

        loop {
            ticker.tick().await;

            // Only send audio when we have a full input frame.
            // If Spotify stalls briefly, we send Opus silence to keep timing stable.
            if consumer.len() < frame_in_samples {
                underflow_frames += 1;
                consecutive_underflows += 1;

                // After a longer stall, end transmission once so Mumble clients don't get "stuck".
                if stream_active && consecutive_underflows * MUMBLE_FRAME_DURATION_MS >= 600 {
                    if let Ok(opus_data) = encoder.encode_vec_float(&silence_pcm, MUMBLE_FRAME_SIZE) {
                        let packet = VoicePacket::Audio {
                            _dst: std::marker::PhantomData,
                            target: 0,
                            session_id: (),
                            seq_num,
                            payload: VoicePacketPayload::Opus(opus_data.into(), true),
                            position_info: None,
                        };
                        let _ = udp_voice_tx_audio.try_send(packet);
                        seq_num += 1;
                    }
                    stream_active = false;
                } else if stream_active {
                    // Short stall: send silence, let Opus PLC/FEC do its thing.
                    if let Ok(opus_data) = encoder.encode_vec_float(&silence_pcm, MUMBLE_FRAME_SIZE) {
                        let packet = VoicePacket::Audio {
                            _dst: std::marker::PhantomData,
                            target: 0,
                            session_id: (),
                            seq_num,
                            payload: VoicePacketPayload::Opus(opus_data.into(), false),
                            position_info: None,
                        };
                        let _ = udp_voice_tx_audio.try_send(packet);
                        seq_num += 1;
                    }
                }

                if last_underflow_log.elapsed() >= Duration::from_secs(5) {
                    warn!("Audio underflow: missing {} frame(s) in last ~5s", underflow_frames);
                    underflow_frames = 0;
                    last_underflow_log = time::Instant::now();
                }
                continue;
            }

            consecutive_underflows = 0;
            stream_active = true;

            consumer.pop_slice(&mut input_buffer_interleaved);

            for i in 0..SPOTIFY_FRAME_SIZE {
                input_buffer_planar[0][i] = input_buffer_interleaved[2 * i];
                input_buffer_planar[1][i] = input_buffer_interleaved[2 * i + 1];
            }

            let out = match resampler.process(&input_buffer_planar, None) {
                Ok(v) => v,
                Err(e) => {
                    error!("Resampler error: {e}");
                    continue;
                }
            };

            for (ch_idx, ch_data) in out.iter().enumerate() {
                let n = std::cmp::min(ch_data.len(), MUMBLE_FRAME_SIZE);
                output_buffer_planar[ch_idx][..n].copy_from_slice(&ch_data[..n]);
                if n < MUMBLE_FRAME_SIZE {
                    output_buffer_planar[ch_idx][n..].fill(0.0);
                }
            }

            for i in 0..MUMBLE_FRAME_SIZE {
                output_buffer_interleaved[2 * i] = output_buffer_planar[0][i];
                output_buffer_interleaved[2 * i + 1] = output_buffer_planar[1][i];
            }

            // Opus expects frame_size = samples per channel (not interleaved length).
            match encoder.encode_vec_float(&output_buffer_interleaved, MUMBLE_FRAME_SIZE) {
                Ok(opus_data) => {
                    let packet = VoicePacket::Audio {
                        _dst: std::marker::PhantomData,
                        target: 0,
                        session_id: (),
                        seq_num,
                        payload: VoicePacketPayload::Opus(opus_data.into(), false),
                        position_info: None,
                    };

                    // Send via UDP task (encrypted UDP). Drop if queue is full to avoid lag.
                    let _ = udp_voice_tx_audio.try_send(packet);
                    seq_num += 1;
                }
                Err(e) => error!("Opus encoding error: {}", e),
            }
        }
    });

    // 6. Main Control Loop (Handles Ping + Commands + Audio Tunneling)
    info!("Mumble Client connected. Waiting for sync...");
    let mut ping_interval = time::interval(Duration::from_secs(10));
    
    loop {
        tokio::select! {
            // Heartbeat
            _ = ping_interval.tick() => {
                let mut ping = mumble_protocol::control::msgs::Ping::new();
                ping.set_timestamp(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64);
                let _ = control_tx.send(ControlPacket::Ping(Box::new(ping))).await;
            }

            // Incoming Packets
            packet_option = control_rx.next() => {
                match packet_option {
                    Some(Ok(packet)) => {
                        match packet {
                            ControlPacket::CryptSetup(msg) => {
                                let state = ClientCryptState::new_from(
                                    msg.get_key()
                                        .try_into()
                                        .expect("Server sent private key with incorrect size"),
                                    msg.get_client_nonce()
                                        .try_into()
                                        .expect("Server sent client_nonce with incorrect size"),
                                    msg.get_server_nonce()
                                        .try_into()
                                        .expect("Server sent server_nonce with incorrect size"),
                                );

                                if control_synced {
                                    let _ = crypt_state_tx.try_send(state);
                                } else {
                                    pending_crypt_state = Some(state);
                                }
                            }
                            ControlPacket::TextMessage(msg) => {
                                let text = msg.get_message();
                                if text.starts_with("!") {
                                    let parts: Vec<&str> = text.split_whitespace().collect();
                                    match parts[0] {
                                        "!play" => info!("!play ignored (Use Spotify Connect)"),
                                        "!stop" => { let _ = cmd_tx.send(SpotifyCommand::Stop); },
                                        "!vol" => {
                                            if parts.len() > 1 {
                                                if let Ok(v) = parts[1].parse::<u16>() {
                                                    let _ = cmd_tx.send(SpotifyCommand::Volume(v));
                                                }
                                            }
                                        },
                                        _ => {}
                                    }
                                }
                            },
                            ControlPacket::ServerSync(sync) => {
                                _session_id = Some(sync.get_session());
                                info!("Logged in as session {}. Ready!", sync.get_session());

                                control_synced = true;
                                if let Some(state) = pending_crypt_state.take() {
                                    let _ = crypt_state_tx.try_send(state);
                                }
                            },
                            ControlPacket::Ping(_ping) => {
                                // Reply to server ping?
                                // Usually we just ignore, we send our own.
                            },
                            _ => {}
                        }
                    },
                    Some(Err(e)) => {
                        error!("Mumble connection error: {}", e);
                        break;
                    },
                    None => {
                        info!("Mumble connection closed");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}