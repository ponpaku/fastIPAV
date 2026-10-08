use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformProfile {
    #[default]
    Auto,
    LinuxPc,
    RaspberryPi,
}

impl PlatformProfile {
    pub fn resolve(&self) -> Self {
        match self {
            Self::Auto if running_on_raspberry_pi() => Self::RaspberryPi,
            Self::Auto => Self::LinuxPc,
            other => other.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererKind {
    #[default]
    Auto,
    Sdl,
    KmsDrm,
}

impl RendererKind {
    pub fn resolve(&self, profile: &PlatformProfile) -> Self {
        match self {
            Self::Auto => match profile.resolve() {
                PlatformProfile::RaspberryPi => Self::KmsDrm,
                PlatformProfile::LinuxPc | PlatformProfile::Auto => Self::Sdl,
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

    fn validate(&self, audio_enabled: bool) -> Result<()> {
        let multicast_group: Ipv4Addr = self
            .multicast_group
            .parse()
            .with_context(|| format!("invalid IPv4 multicast group {}", self.multicast_group))?;
        if !multicast_group.is_multicast() {
            bail!("network.multicast_group must be an IPv4 multicast address");
        }
        if let Some(interface) = self.interface_override() {
            crate::net::validate_interface_name(interface)?;
        }
        if self.video_port == 0 {
            bail!("network.video_port must be greater than zero");
        }
        if audio_enabled && self.audio_port == 0 {
            bail!("network.audio_port must be greater than zero when audio is enabled");
        }
        if audio_enabled && self.video_port == self.audio_port {
            bail!("network.video_port and network.audio_port must differ when audio is enabled");
        }
        if !(96..=127).contains(&self.video_payload_type) {
            bail!("network.video_payload_type must be a dynamic RTP payload type in 96..=127");
        }
        if audio_enabled && !(96..=127).contains(&self.audio_payload_type) {
            bail!("network.audio_payload_type must be a dynamic RTP payload type in 96..=127");
        }
        if !(1..=255).contains(&self.ttl) {
            bail!("network.ttl must be in 1..=255");
        }
        if self.rtp_mtu < 28 {
            bail!("network.rtp_mtu must be at least 28 bytes");
        }
        if self.rtp_mtu > 65_507 {
            bail!("network.rtp_mtu must not exceed the IPv4 UDP payload limit of 65507 bytes");
        }
        if self.receive_buffer_size > i32::MAX as u32 {
            bail!("network.receive_buffer_size must fit in a signed 32-bit GStreamer property");
        }
        if self.receive_buffer_size != 0 && self.receive_buffer_size < self.rtp_mtu {
            bail!(
                "network.receive_buffer_size must be zero or at least network.rtp_mtu ({} bytes)",
                self.rtp_mtu
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
        let ip: IpAddr = self
            .bind_addr
            .parse()
            .with_context(|| format!("invalid HTTP bind IP address {}", self.bind_addr))?;
        Ok(SocketAddr::new(ip, self.port))
    }

    fn validate(&self) -> Result<()> {
        if self.port == 0 {
            bail!("http.port must be greater than zero");
        }
        self.socket_addr().map(|_| ())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryConfig {
    #[serde(default = "default_restart_backoff_ms")]
    pub restart_backoff_ms: u64,
    #[serde(default = "default_monitor_interval_ms")]
    pub monitor_interval_ms: u64,
    #[serde(default = "default_media_timeout_ms")]
    pub media_timeout_ms: u64,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            restart_backoff_ms: default_restart_backoff_ms(),
            monitor_interval_ms: default_monitor_interval_ms(),
            media_timeout_ms: default_media_timeout_ms(),
        }
    }
}

impl RecoveryConfig {
    fn validate(&self) -> Result<()> {
        if self.restart_backoff_ms == 0 {
            bail!("recovery.restart_backoff_ms must be greater than zero");
        }
        if self.monitor_interval_ms == 0 {
            bail!("recovery.monitor_interval_ms must be greater than zero");
        }
        if self.media_timeout_ms == 0 {
            bail!("recovery.media_timeout_ms must be greater than zero");
        }
        if self.media_timeout_ms <= self.monitor_interval_ms {
            bail!("recovery.media_timeout_ms must be greater than recovery.monitor_interval_ms");
        }
        Ok(())
    }
}

/// V4L2 capture transfer mode. `auto` preserves GStreamer's device-specific
/// choice, while `mmap` avoids the userspace read() copy on supported devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureIoMode {
    #[default]
    Auto,
    Rw,
    Mmap,
    Userptr,
    Dmabuf,
    DmabufImport,
}

impl CaptureIoMode {
    pub fn gst_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Rw => "rw",
            Self::Mmap => "mmap",
            Self::Userptr => "userptr",
            Self::Dmabuf => "dmabuf",
            Self::DmabufImport => "dmabuf-import",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TxVideoConfig {
    #[serde(default)]
    pub source_element: String,
    #[serde(default)]
    pub source_caps: String,
    #[serde(default)]
    pub source_decoder_element: String,
    #[serde(default = "default_video_device")]
    pub device: String,
    #[serde(default)]
    pub capture_io_mode: CaptureIoMode,
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
            source_caps: String::new(),
            source_decoder_element: String::new(),
            device: default_video_device(),
            capture_io_mode: CaptureIoMode::Auto,
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
#[serde(deny_unknown_fields)]
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
    #[serde(default = "default_video_sink_sync")]
    pub sync: bool,
    #[serde(default = "default_video_max_lateness_ms")]
    pub max_lateness_ms: i64,
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
            sync: default_video_sink_sync(),
            max_lateness_ms: default_video_max_lateness_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TxAudioConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub source_element: String,
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
            source_element: String::new(),
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
#[serde(deny_unknown_fields)]
pub struct RxAudioConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub sink_element: String,
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
            sink_element: String::new(),
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
#[serde(deny_unknown_fields)]
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
        let config: Self = load_toml(path)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        validate_node_name(&self.node_name)?;
        self.network.validate(self.audio.enabled)?;
        self.http.validate()?;
        self.recovery.validate()?;
        validate_video_dimensions(self.video.width, self.video.height, self.video.fps)?;
        validate_media_timeout(self.recovery.media_timeout_ms, self.video.fps)?;

        let encoder = self.video.encoder_element.trim();
        let managed_encoder = encoder.is_empty()
            || encoder == "auto"
            || (!encoder.contains('!')
                && encoder
                    .split_whitespace()
                    .next()
                    .is_some_and(|name| name == "x264enc"));
        if managed_encoder
            && (!self.video.width.is_multiple_of(2) || !self.video.height.is_multiple_of(2))
        {
            bail!("video width and height must be even for automatic/x264 I420/NV12 encoding");
        }

        if managed_encoder {
            if self.video.bitrate_kbps == 0 {
                bail!("video.bitrate_kbps must be greater than zero for automatic/x264 encoding");
            }
            if (self.video.bitrate_kbps as u64) * 1_000 > i32::MAX as u64 {
                bail!("video.bitrate_kbps is too large for automatic V4L2 bitrate controls");
            }
            if self.video.gop == 0 {
                bail!("video.gop must be greater than zero for automatic/x264 encoding");
            }
            if self.video.gop > i32::MAX as u32 {
                bail!("video.gop is too large for automatic V4L2 encoder controls");
            }
            let keyframe_interval_ms = (self.video.gop as u64)
                .saturating_mul(1_000)
                .div_ceil(self.video.fps as u64);
            if keyframe_interval_ms >= self.recovery.media_timeout_ms {
                bail!(
                    "video.gop implies a keyframe interval of about {} ms, which must be smaller than recovery.media_timeout_ms ({} ms) for packet-loss recovery",
                    keyframe_interval_ms,
                    self.recovery.media_timeout_ms
                );
            }
        }
        if self.video.source_element.trim().is_empty() && self.video.device.trim().is_empty() {
            bail!("video.device must not be empty when video.source_element is not set");
        }
        if self.audio.enabled {
            validate_audio(
                self.audio.sample_rate,
                self.audio.channels,
                self.audio.buffer_time_us,
                self.audio.latency_time_us,
            )?;
            if self.audio.source_element.trim().is_empty() && self.audio.device.trim().is_empty() {
                bail!("audio.device must not be empty when audio.source_element is not set");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
        let config: Self = load_toml(path)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        validate_node_name(&self.node_name)?;
        self.network.validate(self.audio.enabled)?;
        self.http.validate()?;
        self.recovery.validate()?;
        validate_video_dimensions(self.video.width, self.video.height, self.video.fps)?;
        validate_media_timeout(self.recovery.media_timeout_ms, self.video.fps)?;
        if self.video.jitter_latency_ms as u64 >= self.recovery.media_timeout_ms {
            bail!(
                "video.jitter_latency_ms ({} ms) must be smaller than recovery.media_timeout_ms ({} ms)",
                self.video.jitter_latency_ms,
                self.recovery.media_timeout_ms
            );
        }
        if self.video.sink_element.trim().is_empty()
            && matches!(
                self.video.renderer.resolve(&self.platform.profile),
                RendererKind::KmsDrm
            )
            && !self.video.fullscreen
        {
            bail!("video.fullscreen=false is not supported with the KMS/DRM renderer");
        }
        if self.video.max_lateness_ms < -1 {
            bail!("video.max_lateness_ms must be -1 (unlimited) or a non-negative value");
        }
        if self.video.max_lateness_ms > i64::MAX / 1_000_000 {
            bail!("video.max_lateness_ms is too large");
        }

        if self.audio.enabled {
            validate_audio(
                self.audio.sample_rate,
                self.audio.channels,
                self.audio.buffer_time_us,
                self.audio.latency_time_us,
            )?;
            if self.audio.sink_element.trim().is_empty() && self.audio.device.trim().is_empty() {
                bail!("audio.device must not be empty when audio.sink_element is not set");
            }
            if self.audio.jitter_latency_ms as u64 >= self.recovery.media_timeout_ms {
                bail!(
                    "audio.jitter_latency_ms ({} ms) must be smaller than recovery.media_timeout_ms ({} ms)",
                    self.audio.jitter_latency_ms,
                    self.recovery.media_timeout_ms
                );
            }
            if self.audio.late_threshold_ms == 0 {
                bail!("audio.late_threshold_ms must be greater than zero when audio is enabled");
            }
            if self.audio.sync_tolerance_ms == 0 {
                bail!("audio.sync_tolerance_ms must be greater than zero when audio is enabled");
            }
            let jitter_delta_ms = self
                .audio
                .jitter_latency_ms
                .abs_diff(self.video.jitter_latency_ms) as u64;
            if jitter_delta_ms >= self.recovery.media_timeout_ms {
                bail!(
                    "audio/video jitter latency difference ({} ms) must be smaller than recovery.media_timeout_ms ({} ms)",
                    jitter_delta_ms,
                    self.recovery.media_timeout_ms
                );
            }
        }
        Ok(())
    }
}

fn validate_node_name(node_name: &str) -> Result<()> {
    if node_name.trim().is_empty() {
        bail!("node_name must not be empty");
    }
    Ok(())
}

fn validate_video_dimensions(width: u32, height: u32, fps: u32) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("video width and height must be greater than zero");
    }
    if width > i32::MAX as u32 || height > i32::MAX as u32 {
        bail!("video width and height must fit in signed 32-bit GStreamer caps");
    }
    if fps == 0 {
        bail!("video.fps must be greater than zero");
    }
    if fps > i32::MAX as u32 {
        bail!("video.fps must fit in signed 32-bit GStreamer caps");
    }
    Ok(())
}

fn validate_media_timeout(media_timeout_ms: u64, fps: u32) -> Result<()> {
    let frame_interval_ms = 1_000_u64.div_ceil(fps as u64);
    if media_timeout_ms <= frame_interval_ms {
        bail!(
            "recovery.media_timeout_ms must be greater than the nominal video frame interval ({} ms at {} fps)",
            frame_interval_ms,
            fps
        );
    }
    Ok(())
}

fn validate_audio(
    sample_rate: u32,
    channels: u32,
    buffer_time_us: i64,
    latency_time_us: i64,
) -> Result<()> {
    if sample_rate == 0 {
        bail!("audio.sample_rate must be greater than zero");
    }
    if sample_rate > i32::MAX as u32 {
        bail!("audio.sample_rate must fit in signed 32-bit GStreamer caps");
    }
    if channels == 0 {
        bail!("audio.channels must be greater than zero");
    }
    if channels > i32::MAX as u32 {
        bail!("audio.channels must fit in signed 32-bit GStreamer caps");
    }
    if buffer_time_us <= 0 {
        bail!("audio.buffer_time_us must be greater than zero");
    }
    if latency_time_us <= 0 {
        bail!("audio.latency_time_us must be greater than zero");
    }
    if latency_time_us > buffer_time_us {
        bail!("audio.latency_time_us must not exceed audio.buffer_time_us");
    }
    Ok(())
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

fn running_on_raspberry_pi() -> bool {
    fs::read("/proc/device-tree/model")
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .is_some_and(|model| model.trim_end_matches('\0').contains("Raspberry Pi"))
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

fn default_media_timeout_ms() -> u64 {
    5000
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
    // A low-latency starting point for wired multicast. Sites with more
    // jitter can raise this at the cost of added capture-to-display delay.
    10
}

fn default_audio_jitter_latency_ms() -> u32 {
    30
}

fn default_encoder_element() -> String {
    "auto".to_string()
}

fn default_decoder_element() -> String {
    "auto".to_string()
}

fn default_fullscreen() -> bool {
    true
}

fn default_video_sink_sync() -> bool {
    // Present newly decoded frames immediately rather than scheduling them
    // against the receiver's (unsynchronized) pipeline clock.
    false
}

fn default_sink_sync() -> bool {
    true
}

fn default_video_max_lateness_ms() -> i64 {
    -1
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
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receiver_defaults_to_immediate_video_rendering() {
        let rx = RxConfig::default();
        assert!(!rx.video.sync);
        // Audio retains its own timing policy; it is not silently changed.
        assert!(rx.audio.sync);
    }

    #[test]
    fn capture_io_mode_is_typed_and_defaults_to_auto() {
        let tx = TxConfig::default();
        assert_eq!(tx.video.capture_io_mode, CaptureIoMode::Auto);
        let serialized = toml::to_string(&tx).unwrap();
        assert!(serialized.contains("capture_io_mode = \"auto\""));
        assert!(toml::from_str::<TxConfig>(
            &serialized.replace("capture_io_mode = \"auto\"", "capture_io_mode = \"bogus\"")
        )
        .is_err());
        for (mode, value) in [
            (CaptureIoMode::Mmap, "mmap"),
            (CaptureIoMode::Rw, "rw"),
            (CaptureIoMode::Userptr, "userptr"),
            (CaptureIoMode::Dmabuf, "dmabuf"),
            (CaptureIoMode::DmabufImport, "dmabuf-import"),
        ] {
            assert_eq!(mode.gst_value(), value);
        }
    }

    #[test]
    fn default_configs_are_valid() {
        TxConfig::default().validate().unwrap();
        RxConfig::default().validate().unwrap();
    }

    #[test]
    fn repository_config_examples_load_and_validate() {
        let config_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../configs");

        for name in ["tx.default.toml", "tx.pi.toml", "tx.smoketest.toml"] {
            TxConfig::load(config_dir.join(name))
                .unwrap_or_else(|err| panic!("failed to load {name}: {err:#}"));
        }
        for name in ["rx.default.toml", "rx.pi.toml", "rx.smoketest.toml"] {
            RxConfig::load(config_dir.join(name))
                .unwrap_or_else(|err| panic!("failed to load {name}: {err:#}"));
        }
    }

    #[test]
    fn supports_ipv6_http_bind_address() {
        let config = HttpConfig {
            bind_addr: "::1".to_string(),
            port: 8080,
        };
        assert_eq!(config.socket_addr().unwrap().to_string(), "[::1]:8080");
    }

    #[test]
    fn rejects_invalid_interface_names_in_tx_and_rx_configs() {
        for invalid in ["eth0/../lo", "..", "some interface", "0123456789abcdef"] {
            let mut tx = TxConfig::default();
            tx.network.interface = invalid.to_string();
            assert!(tx.validate().is_err(), "TX accepted {invalid:?}");

            let mut rx = RxConfig::default();
            rx.network.interface = invalid.to_string();
            assert!(rx.validate().is_err(), "RX accepted {invalid:?}");
        }
    }

    #[test]
    fn vlan_interface_name_is_accepted_during_config_validation() {
        let mut config = TxConfig::default();
        config.network.interface = "eth0.100".to_string();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rejects_unicast_group() {
        let mut config = TxConfig::default();
        config.network.multicast_group = "192.168.1.10".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn custom_sink_bypasses_renderer_fullscreen_constraint() {
        let mut config = RxConfig::default();
        config.platform.profile = PlatformProfile::RaspberryPi;
        config.video.sink_element = "fakesink sync=false".to_string();
        config.video.fullscreen = false;
        config.validate().unwrap();
    }

    #[test]
    fn rejects_invalid_negative_video_max_lateness() {
        let mut config = RxConfig::default();
        config.video.max_lateness_ms = -2;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_odd_video_dimensions() {
        let mut config = TxConfig::default();
        config.video.width = 1919;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_zero_fps() {
        let mut config = RxConfig::default();
        config.video.fps = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_static_payload_type_for_h264() {
        let mut config = TxConfig::default();
        config.network.video_payload_type = 35;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_invalid_multicast_ttl() {
        let mut config = TxConfig::default();
        config.network.ttl = 256;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_video_bitrate_above_v4l2_control_range() {
        let mut config = TxConfig::default();
        config.video.bitrate_kbps = (i32::MAX as u32 / 1_000) + 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_keyframe_interval_longer_than_media_timeout() {
        let mut config = TxConfig::default();
        config.video.fps = 30;
        config.video.gop = 150;
        config.recovery.media_timeout_ms = 5_000;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_windowed_kms_renderer() {
        let mut config = RxConfig::default();
        config.platform.profile = PlatformProfile::RaspberryPi;
        config.video.renderer = RendererKind::KmsDrm;
        config.video.fullscreen = false;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rx_rejects_jitter_latency_at_or_above_media_timeout() {
        let mut config = RxConfig::default();
        config.video.jitter_latency_ms = config.recovery.media_timeout_ms as u32;
        assert!(config.validate().is_err());

        let mut audio_config = RxConfig::default();
        audio_config.audio.enabled = true;
        audio_config.audio.jitter_latency_ms = audio_config.recovery.media_timeout_ms as u32;
        assert!(audio_config.validate().is_err());
    }

    #[test]
    fn custom_tx_encoder_does_not_require_managed_bitrate_or_gop() {
        let mut config = TxConfig::default();
        config.video.encoder_element = "customh264enc low-latency=true".to_string();
        config.video.bitrate_kbps = 0;
        config.video.gop = 0;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn custom_tx_encoder_may_use_odd_dimensions() {
        let mut config = TxConfig::default();
        config.video.width = 641;
        config.video.height = 481;
        config.video.encoder_element = "identity".to_string();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rejects_media_timeout_shorter_than_frame_interval() {
        let mut config = TxConfig::default();
        config.recovery.monitor_interval_ms = 1;
        config.recovery.media_timeout_ms = 20;
        config.video.fps = 30;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_video_caps_values_above_gstreamer_int_range() {
        let mut config = RxConfig::default();
        config.video.width = (i32::MAX as u32) + 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_jitter_difference_that_exceeds_media_timeout() {
        let mut config = RxConfig::default();
        config.audio.enabled = true;
        config.video.jitter_latency_ms = 0;
        config.audio.jitter_latency_ms = 6_000;
        config.recovery.media_timeout_ms = 5_000;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_audio_latency_larger_than_buffer() {
        let mut config = TxConfig::default();
        config.audio.enabled = true;
        config.audio.buffer_time_us = 5_000;
        config.audio.latency_time_us = 10_000;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_audio_caps_values_above_gstreamer_int_range() {
        let mut config = TxConfig::default();
        config.audio.enabled = true;
        config.audio.sample_rate = (i32::MAX as u32) + 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_zero_multicast_ttl() {
        let mut config = TxConfig::default();
        config.network.ttl = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_too_small_rtp_mtu() {
        let mut config = TxConfig::default();
        config.network.rtp_mtu = 27;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_rtp_mtu_above_ipv4_udp_limit() {
        let mut config = TxConfig::default();
        config.network.rtp_mtu = 65_508;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_receive_buffer_smaller_than_rtp_packet() {
        let mut config = RxConfig::default();
        config.network.rtp_mtu = 1200;
        config.network.receive_buffer_size = 1199;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_oversized_receive_buffer() {
        let mut config = RxConfig::default();
        config.network.receive_buffer_size = (i32::MAX as u32) + 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_shared_rtp_port_when_audio_is_enabled() {
        let mut config = TxConfig::default();
        config.audio.enabled = true;
        config.network.audio_port = config.network.video_port;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_unknown_config_fields() {
        let input = r#"
node_name = "test"

[video]
fps = 30
typo_fps = 60
"#;
        assert!(toml::from_str::<TxConfig>(input).is_err());
    }
}
