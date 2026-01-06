use anyhow::Result;
use futures_util::{StreamExt, SinkExt};
use librespot_core::config::SessionConfig;
use librespot_core::session::Session;
use librespot_core::spotify_id::SpotifyId;
use librespot_playback::audio_backend::{Sink, SinkError};
use librespot_playback::config::PlayerConfig;
use librespot_playback::mixer::{Mixer, MixerConfig};
use librespot_playback::decoder::AudioPacket;
use librespot_connect::spirc::Spirc;
use librespot_playback::player::Player;
use log::{error, info, warn};
use mumble_protocol::control::{ClientControlCodec, ControlPacket};
use mumble_protocol::voice::{VoicePacket, VoicePacketPayload};
use opus::{Application, Channels, Encoder};
use ringbuf::{HeapRb, Producer};
use rubato::{FftFixedIn, Resampler};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time;
use tokio_native_tls::native_tls::TlsConnector;
use tokio_util::codec::Decoder;
use librespot_playback::mixer::VolumeGetter;
use librespot_discovery::Discovery;

// Audio Constants
const MUMBLE_SAMPLE_RATE: usize = 48000;
const MUMBLE_FRAME_DURATION_MS: u64 = 40;
const MUMBLE_FRAME_SIZE: usize = (MUMBLE_SAMPLE_RATE as u64 * MUMBLE_FRAME_DURATION_MS / 1000) as usize;
// const SPOTIFY_FRAME_SIZE: usize = 882;
const SPOTIFY_FRAME_SIZE: usize = 882*(MUMBLE_FRAME_DURATION_MS/20) as usize;

const CHANNELS: usize = 2;
const RINGBUF_SIZE: usize = SPOTIFY_FRAME_SIZE * CHANNELS * 10; 

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
        unimplemented!()
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
                let samples_f32: Vec<f32> = samples.iter().map(|&s| s as f32).collect();
                let mut written = 0;
                while written < samples_f32.len() {
                    let n = self.producer.push_slice(&samples_f32[written..]);
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
        Load(String),
        Play,
        Pause,
        Stop,
        Volume(u16),
    }
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SpotifyCommand>();
    
    // Channel for Audio Packets (AudioThread -> MainThread)
    // Send (seq_num, opus_data)
    let (audio_tx, mut audio_rx) = mpsc::channel::<(u64, Vec<u8>)>(100);

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

                    let sink = Box::new(RingbufSink { producer });
                    let backend_sink = Arc::new(Mutex::new(Some(sink)));
                    
                    let backend_fn = move || {
                        let s = backend_sink.lock().unwrap().take().expect("Sink already taken");
                        s as Box<dyn Sink>
                    };

                    let (session_obj, _creds) = session;

                    let soft_mixer = librespot_playback::mixer::softmixer::SoftMixer::open(mixer_config);
                    let shared_mixer = SharedMixer { inner: Arc::new(Mutex::new(soft_mixer)) };

                    struct NoOpVolume;
                    impl VolumeGetter for NoOpVolume {
                         fn attenuation_factor(&self) -> f64 { 1.0 }
                    }

                    let (player, mut event_channel) = Player::new(
                        player_config,
                        session_obj.clone(),
                        Box::new(NoOpVolume),
                        backend_fn
                    );

                    let (_spirc, spirc_task) = Spirc::new(
                        Default::default(),
                        session_obj,
                        player,
                        Box::new(shared_mixer.clone())
                    );
                    
                    tokio::spawn(spirc_task);

                    tokio::spawn(async move {
                        while let Some(event) = event_channel.recv().await {
                            info!("Spotify Event: {:?}", event);
                        }
                    });

                    // Command Loop
                    while let Some(cmd) = cmd_rx.recv().await {
                        match cmd {
                            SpotifyCommand::Load(_uri_str) => {
                                info!("Local !play command ignored (Spirc owns Player). Use Spotify Connect.");
                            },
                            SpotifyCommand::Play => info!("Local !play ignored."),
                            SpotifyCommand::Pause => info!("Local !pause ignored."),
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
    let mumble_server = "mumble.gameone.dev:64738";
    let mumble_user = "MumbleDJ";

    info!("Connecting to Mumble server: {}", mumble_server);
    let socket = tokio::net::TcpStream::connect(mumble_server).await?;
    let cx = TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .build()?;
    let tokio_cx = tokio_native_tls::TlsConnector::from(cx);
    let tls_stream = tokio_cx.connect(mumble_server, socket).await?;

    let (mut control_tx, mut control_rx) = ClientControlCodec::new().framed(tls_stream).split();

    let mut version = mumble_protocol::control::msgs::Version::new();
    version.set_version(1 << 16 | 3 << 8 | 0);
    version.set_release("Mumtify 0.1".into());
    version.set_os("Rust".into());
    control_tx.send(ControlPacket::Version(Box::new(version))).await?;

    let mut authenticate = mumble_protocol::control::msgs::Authenticate::new();
    authenticate.set_username(mumble_user.into());
    control_tx.send(ControlPacket::Authenticate(Box::new(authenticate))).await?;

    let mut _session_id = None;
    
    // 5. Audio Processing Loop
    tokio::spawn(async move {
        let mut encoder = Encoder::new(MUMBLE_SAMPLE_RATE as u32, Channels::Stereo, Application::Audio).unwrap();
        encoder.set_bitrate(opus::Bitrate::Bits(128000)).unwrap();

        let mut resampler = FftFixedIn::<f32>::new(SPOTIFY_FRAME_SIZE, MUMBLE_FRAME_SIZE, SPOTIFY_FRAME_SIZE, 1, CHANNELS).unwrap();
        
        let mut input_buffer_interleaved = vec![0.0; SPOTIFY_FRAME_SIZE * CHANNELS];
        let mut input_buffer_planar = vec![vec![0.0; SPOTIFY_FRAME_SIZE]; CHANNELS];
        let mut output_buffer_planar = vec![vec![0.0; MUMBLE_FRAME_SIZE]; CHANNELS];
        let mut output_buffer_interleaved = vec![0.0; MUMBLE_FRAME_SIZE * CHANNELS];
        
        let mut seq_num = 0u64;
        let mut ticker = time::interval(Duration::from_millis(MUMBLE_FRAME_DURATION_MS));

        loop {
            ticker.tick().await;

            if consumer.len() >= SPOTIFY_FRAME_SIZE * CHANNELS {
                consumer.pop_slice(&mut input_buffer_interleaved);

                for i in 0..SPOTIFY_FRAME_SIZE {
                    input_buffer_planar[0][i] = input_buffer_interleaved[2*i];
                    input_buffer_planar[1][i] = input_buffer_interleaved[2*i+1];
                }

                let out = resampler.process(&input_buffer_planar, None).unwrap();
                
                for (ch_idx, ch_data) in out.iter().enumerate() {
                     output_buffer_planar[ch_idx].copy_from_slice(ch_data);
                }

                for i in 0..MUMBLE_FRAME_SIZE {
                    output_buffer_interleaved[2*i] = output_buffer_planar[0][i];
                    output_buffer_interleaved[2*i+1] = output_buffer_planar[1][i];
                }

                match encoder.encode_vec_float(&output_buffer_interleaved, MUMBLE_FRAME_SIZE * CHANNELS) {
                    Ok(opus_data) => {
                        // Send to Main Loop to wrap in UDPTunnel
                        if let Err(_) = audio_tx.send((seq_num, opus_data)).await {
                            break;
                        }
                        seq_num += 1;
                    },
                    Err(e) => error!("Opus encoding error: {}", e),
                }
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

            // Audio via TCP Tunneling
            Some((seq_num, opus_data)) = audio_rx.recv() => {
                let packet = VoicePacket::Audio {
                    _dst: std::marker::PhantomData,
                    target: 0,
                    session_id: (),
                    seq_num,
                    payload: VoicePacketPayload::Opus(opus_data.into(), false),
                    position_info: None,
                };
                let _ = control_tx.send(ControlPacket::UDPTunnel(Box::new(packet))).await;
            }

            // Incoming Packets
            packet_option = control_rx.next() => {
                match packet_option {
                    Some(Ok(packet)) => {
                        match packet {
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
                            },
                            ControlPacket::Ping(ping) => {
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