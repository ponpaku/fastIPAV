use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, net::SocketAddr, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformProfile {
    Auto,
    LinuxPc,
    RaspberryPi,
}

impl Default for PlatformProfile {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererKind {
    Auto,
    Sdl,
    KmsDrm,
}

impl Default for RendererKind {
    fn default() -> Self {
        Self::Auto
    }
}

impl RendererKind {
    pub fn resolve(&self, profile: &PlatformProfile) -> Self {
        match self {
            Self::Auto => match profile {
                PlatformProfile::RaspberryPi => Self::KmsDrm,
                _ => Self::Sdl,
            },
            other => other.clone(),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Sdl => "sdl",
            Self::KmsDrm => "kms_drm",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformConfig {
    #[serde(default)]
    pub profile: PlatformProfile,
}

impl Default for PlatformConfig {
    fn default() -> Self {
        Self {
            profile: PlatformProfile::Auto,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    #[serde(default = "default_multicast_group")]
    pub multicast_group: String,
    #[serde(default = "default_video_port")]
    pub video_port: u16,
    #[serde(default = "default_audio_port")]
    pub audio_port: u16,
    #[serde(default = "default_interface")]
    pub interface: String,
    #[serde(default = "default_ttl")]
    pub ttl: u32,
    #[serde(default = "default_video_payload_type")]
    pub video_payload_type: u8,
    #[serde(default = "default_audio_payload_type")]
    pub audio_payload_type: u8,
    #[serde(default = "default_rtp_mtu")]
    pub rtp_mtu: u32,
    #[serde(default)]
    pub receive_buffer_size: u32,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            multicast_group: default_multicast_group(),
            video_port: default_video_port(),
            audio_port: default_audio_port(),
            interface: default_interface(),
            ttl: default_ttl(),
            video_payload_type: default_video_payload_type(),
            audio_payload_type: default_audio_payload_type(),
            rtp_mtu: default_rtp_mtu(),
            receive_buffer_size: 0,
        }
    }
}

impl NetworkConfig {
    pub fn interface_override(&self) -> Option<&str> {
        match self.interface.trim() {
            "" | "auto" => None,
            explicit => Some(explicit),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    #[serde(default = "default_http_bind_addr")]
    pub bind_addr: String,
    #[serde(default = "default_http_port")]
    pub port: u16,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            bind_addr: default_http_bind_addr(),
            port: default_http_port(),
        }
    }
}

impl HttpConfig {
    pub fn socket_addr(&self) -> Result<SocketAddr> {
        format!("{}:{}", self.bind_addr, self.port)
            .parse()
            .with_context(|| format!("invalid http bind address {}:{}", self.bind_addr, self.port))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryConfig {
    #[serde(default = "default_restart_backoff_ms")]
    pub restart_backoff_ms: u64,
    #[serde(default = "default_monitor_interval_ms")]
    pub monitor_interval_ms: u64,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            restart_backoff_ms: default_restart_backoff_ms(),
            monitor_interval_ms: default_monitor_interval_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxVideoConfig {
    #[serde(default)]
    pub source_element: String,
    #[serde(default = "default_video_device")]
    pub device: String,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default = "default_bitrate_kbps")]
    pub bitrate_kbps: u32,
    #[serde(default = "default_gop")]
    pub gop: u32,
    #[serde(default = "default_encoder_element")]
    pub encoder_element: String,
}

impl Default for TxVideoConfig {
    fn default() -> Self {
        Self {
            source_element: String::new(),
            device: default_video_device(),
            width: default_width(),
            height: default_height(),
            fps: default_fps(),
            bitrate_kbps: default_bitrate_kbps(),
            gop: default_gop(),
            encoder_element: default_encoder_element(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RxVideoConfig {
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default = "default_video_jitter_latency_ms")]
    pub jitter_latency_ms: u32,
    #[serde(default = "default_decoder_element")]
    pub decoder_element: String,
    #[serde(default)]
    pub sink_element: String,
    #[serde(default)]
    pub renderer: RendererKind,
    #[serde(default = "default_fullscreen")]
    pub fullscreen: bool,
    #[serde(default = "default_sink_sync")]
    pub sync: bool,
    #[serde(default = "default_video_max_lateness_ms")]
    pub max_lateness_ms: u32,
}

impl Default for RxVideoConfig {
    fn default() -> Self {
        Self {
            width: default_width(),
            height: default_height(),
            fps: default_fps(),
            jitter_latency_ms: default_video_jitter_latency_ms(),
            decoder_element: default_decoder_element(),
            sink_element: String::new(),
            renderer: RendererKind::Auto,
            fullscreen: default_fullscreen(),
            sync: default_sink_sync(),
            max_lateness_ms: default_video_max_lateness_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxAudioConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_audio_device")]
    pub device: String,
    #[serde(default = "default_audio_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_audio_channels")]
    pub channels: u32,
    #[serde(default = "default_audio_buffer_time_us")]
    pub buffer_time_us: i64,
    #[serde(default = "default_audio_latency_time_us")]
    pub latency_time_us: i64,
    #[serde(default = "default_use_driver_timestamps")]
    pub use_driver_timestamps: bool,
}

impl Default for TxAudioConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            device: default_audio_device(),
            sample_rate: default_audio_sample_rate(),
            channels: default_audio_channels(),
            buffer_time_us: default_audio_buffer_time_us(),
            latency_time_us: default_audio_latency_time_us(),
            use_driver_timestamps: default_use_driver_timestamps(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RxAudioConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_audio_device")]
    pub device: String,
    #[serde(default = "default_audio_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_audio_channels")]
    pub channels: u32,
    #[serde(default = "default_audio_jitter_latency_ms")]
    pub jitter_latency_ms: u32,
    #[serde(default = "default_audio_buffer_time_us")]
    pub buffer_time_us: i64,
    #[serde(default = "default_audio_latency_time_us")]
    pub latency_time_us: i64,
    #[serde(default = "default_sink_sync")]
    pub sync: bool,
    #[serde(default = "default_audio_late_threshold_ms")]
    pub late_threshold_ms: u32,
    #[serde(default = "default_audio_sync_tolerance_ms")]
    pub sync_tolerance_ms: u32,
}

impl Default for RxAudioConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            device: default_audio_device(),
            sample_rate: default_audio_sample_rate(),
            channels: default_audio_channels(),
            jitter_latency_ms: default_audio_jitter_latency_ms(),
            buffer_time_us: default_audio_buffer_time_us(),
            latency_time_us: default_audio_latency_time_us(),
            sync: default_sink_sync(),
            late_threshold_ms: default_audio_late_threshold_ms(),
            sync_tolerance_ms: default_audio_sync_tolerance_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxConfig {
    #[serde(default = "default_tx_node_name")]
    pub node_name: String,
    #[serde(default)]
    pub platform: PlatformConfig,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default = "default_tx_http")]
    pub http: HttpConfig,
    #[serde(default)]
    pub recovery: RecoveryConfig,
    #[serde(default)]
    pub video: TxVideoConfig,
    #[serde(default)]
    pub audio: TxAudioConfig,
}

impl Default for TxConfig {
    fn default() -> Self {
        Self {
            node_name: default_tx_node_name(),
            platform: PlatformConfig::default(),
            network: NetworkConfig::default(),
            http: default_tx_http(),
            recovery: RecoveryConfig::default(),
            video: TxVideoConfig::default(),
            audio: TxAudioConfig::default(),
        }
    }
}

impl TxConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        load_toml(path)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RxConfig {
    #[serde(default = "default_rx_node_name")]
    pub node_name: String,
    #[serde(default)]
    pub platform: PlatformConfig,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default = "default_rx_http")]
    pub http: HttpConfig,
    #[serde(default)]
    pub recovery: RecoveryConfig,
    #[serde(default)]
    pub video: RxVideoConfig,
    #[serde(default)]
    pub audio: RxAudioConfig,
}

impl Default for RxConfig {
    fn default() -> Self {
        Self {
            node_name: default_rx_node_name(),
            platform: PlatformConfig::default(),
            network: NetworkConfig::default(),
            http: default_rx_http(),
            recovery: RecoveryConfig::default(),
            video: RxVideoConfig::default(),
            audio: RxAudioConfig::default(),
        }
    }
}

impl RxConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        load_toml(path)
    }
}

fn load_toml<T, P>(path: P) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
    P: AsRef<Path>,
{
    let path = path.as_ref();
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    toml::from_str(&contents)
        .with_context(|| format!("failed to parse TOML from {}", path.display()))
}

fn default_tx_node_name() -> String {
    "avoverip-tx".to_string()
}

fn default_rx_node_name() -> String {
    "avoverip-rx".to_string()
}

fn default_multicast_group() -> String {
    "239.255.10.10".to_string()
}

fn default_video_port() -> u16 {
    5004
}

fn default_audio_port() -> u16 {
    5006
}

fn default_interface() -> String {
    "auto".to_string()
}

fn default_ttl() -> u32 {
    1
}

fn default_video_payload_type() -> u8 {
    96
}

fn default_audio_payload_type() -> u8 {
    97
}

fn default_rtp_mtu() -> u32 {
    1200
}

fn default_http_bind_addr() -> String {
    "127.0.0.1".to_string()
}

fn default_http_port() -> u16 {
    8080
}

fn default_tx_http() -> HttpConfig {
    HttpConfig {
        bind_addr: default_http_bind_addr(),
        port: 8081,
    }
}

fn default_rx_http() -> HttpConfig {
    HttpConfig {
        bind_addr: default_http_bind_addr(),
        port: 8082,
    }
}

fn default_restart_backoff_ms() -> u64 {
    1000
}

fn default_monitor_interval_ms() -> u64 {
    250
}

fn default_video_device() -> String {
    "/dev/video0".to_string()
}

fn default_audio_device() -> String {
    "default".to_string()
}

fn default_width() -> u32 {
    1920
}

fn default_height() -> u32 {
    1080
}

fn default_fps() -> u32 {
    30
}

fn default_bitrate_kbps() -> u32 {
    8000
}

fn default_gop() -> u32 {
    30
}

fn default_video_jitter_latency_ms() -> u32 {
    20
}

fn default_audio_jitter_latency_ms() -> u32 {
    30
}

fn default_encoder_element() -> String {
    "x264enc tune=zerolatency speed-preset=ultrafast".to_string()
}

fn default_decoder_element() -> String {
    "decodebin".to_string()
}

fn default_fullscreen() -> bool {
    true
}

fn default_sink_sync() -> bool {
    true
}

fn default_video_max_lateness_ms() -> u32 {
    25
}

fn default_audio_late_threshold_ms() -> u32 {
    60
}

fn default_audio_sync_tolerance_ms() -> u32 {
    40
}

fn default_audio_sample_rate() -> u32 {
    48_000
}

fn default_audio_channels() -> u32 {
    2
}

fn default_audio_buffer_time_us() -> i64 {
    20_000
}

fn default_audio_latency_time_us() -> i64 {
    5_000
}

fn default_use_driver_timestamps() -> bool {
    true
}
