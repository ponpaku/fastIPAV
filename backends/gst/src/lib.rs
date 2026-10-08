use anyhow::{anyhow, Context, Result};
use avoverip_common::{
    config::{PlatformProfile, RendererKind, RxConfig, TxConfig},
    metrics::pipeline_shape,
};
use gst::prelude::*;
use gstreamer as gst;
use std::{
    env, fs,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::Instant,
};
use tokio::sync::{mpsc, watch};

const BUS_EVENT_CAPACITY: usize = 64;
/// Coalesce high-rate RTP packet probes; watchdog deadlines are measured in
/// seconds, so per-datagram Tokio watch notifications are unnecessary.
const HEARTBEAT_NOTIFY_INTERVAL_MS: u64 = 25;

/// Stores the last emitted notification time (in milliseconds since probe
/// registration). The first packet always notifies the supervisor.
struct HeartbeatNotifier {
    last_reported_ms: AtomicU64,
}

impl HeartbeatNotifier {
    fn new() -> Self {
        Self {
            last_reported_ms: AtomicU64::new(u64::MAX),
        }
    }

    fn should_notify(&self, elapsed_ms: u64) -> bool {
        let previous = self.last_reported_ms.load(Ordering::Relaxed);
        if previous != u64::MAX
            && elapsed_ms.saturating_sub(previous) < HEARTBEAT_NOTIFY_INTERVAL_MS
        {
            return false;
        }
        self.last_reported_ms
            .compare_exchange(previous, elapsed_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }
}

#[derive(Debug, Clone)]
pub enum PipelineEvent {
    Info(String),
    Warning(String),
    Error(String),
    Eos,
    ClockLost,
    Latency,
    AudioUnderrun,
}

impl PipelineEvent {
    pub fn message(&self) -> String {
        match self {
            Self::Info(message) => message.clone(),
            Self::Warning(message) => message.clone(),
            Self::Error(message) => message.clone(),
            Self::Eos => "pipeline reached EOS".to_string(),
            Self::ClockLost => "pipeline lost its clock".to_string(),
            Self::Latency => "pipeline posted latency recalculation".to_string(),
            Self::AudioUnderrun => "audio underrun detected".to_string(),
        }
    }

    pub fn requires_restart(&self) -> bool {
        matches!(self, Self::Error(_) | Self::Eos | Self::ClockLost)
    }
}

#[derive(Debug, Clone)]
pub struct PipelineDescriptions {
    pub full: String,
    pub video: String,
    pub audio: Option<String>,
    pub renderer: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct MediaHeartbeat {
    pub observed_at: Option<Instant>,
    pub total: u64,
}

pub struct PipelineEvents {
    pub bus: mpsc::Receiver<PipelineEvent>,
    pub terminal: watch::Receiver<Option<PipelineEvent>>,
    pub video: watch::Receiver<MediaHeartbeat>,
    pub audio: watch::Receiver<MediaHeartbeat>,
    pub video_codec: watch::Receiver<MediaHeartbeat>,
    pub video_ingress: watch::Receiver<MediaHeartbeat>,
    pub audio_ingress: watch::Receiver<MediaHeartbeat>,
    pub video_egress: watch::Receiver<MediaHeartbeat>,
    pub audio_egress: watch::Receiver<MediaHeartbeat>,
    pub qos: watch::Receiver<u64>,
    _audio_guard: Option<watch::Sender<MediaHeartbeat>>,
    _video_codec_guard: Option<watch::Sender<MediaHeartbeat>>,
    _video_ingress_guard: Option<watch::Sender<MediaHeartbeat>>,
    _audio_ingress_guard: Option<watch::Sender<MediaHeartbeat>>,
    _video_egress_guard: Option<watch::Sender<MediaHeartbeat>>,
    _audio_egress_guard: Option<watch::Sender<MediaHeartbeat>>,
}

pub struct GstServicePipeline {
    name: &'static str,
    descriptions: PipelineDescriptions,
    pipeline: gst::Pipeline,
    stop_flag: Arc<AtomicBool>,
    bus_thread: Option<thread::JoinHandle<()>>,
    bus_poll_interval_ms: u64,
}

impl GstServicePipeline {
    pub fn for_tx(config: &TxConfig, interface_name: Option<&str>) -> Result<Self> {
        Self::for_tx_with_conversion(config, interface_name, false)
    }

    /// Force color conversion after MJPEG decode when the fast I420 path
    /// cannot negotiate the capture device's actual JPEG subsampling.
    pub fn for_tx_with_conversion(
        config: &TxConfig,
        interface_name: Option<&str>,
        force_video_conversion: bool,
    ) -> Result<Self> {
        init_gstreamer()?;
        let descriptions = build_tx_descriptions(config, interface_name, force_video_conversion);
        Self::new("tx", descriptions, config.recovery.monitor_interval_ms)
    }

    pub fn for_rx(config: &RxConfig, interface_name: Option<&str>) -> Result<Self> {
        init_gstreamer()?;
        let descriptions = build_rx_descriptions(config, interface_name);
        Self::new("rx", descriptions, config.recovery.monitor_interval_ms)
    }

    pub fn descriptions(&self) -> &PipelineDescriptions {
        &self.descriptions
    }

    pub fn start(&mut self) -> Result<PipelineEvents> {
        let bus = self
            .pipeline
            .bus()
            .ok_or_else(|| anyhow!("{} pipeline bus is not available", self.name))?;
        let (bus_tx, bus_rx) = mpsc::channel(BUS_EVENT_CAPACITY);
        let (terminal_tx, terminal_rx) = watch::channel(None::<PipelineEvent>);
        let initial_heartbeat = MediaHeartbeat {
            observed_at: None,
            total: 0,
        };
        let (video_tx, video_rx) = watch::channel(initial_heartbeat);
        let (audio_tx, audio_rx) = watch::channel(initial_heartbeat);
        let (video_codec_tx, video_codec_rx) = watch::channel(initial_heartbeat);
        let (video_ingress_tx, video_ingress_rx) = watch::channel(initial_heartbeat);
        let (audio_ingress_tx, audio_ingress_rx) = watch::channel(initial_heartbeat);
        let (video_egress_tx, video_egress_rx) = watch::channel(initial_heartbeat);
        let (audio_egress_tx, audio_egress_rx) = watch::channel(initial_heartbeat);
        let (qos_tx, qos_rx) = watch::channel(0_u64);
        self.install_buffer_probe("video_monitor", video_tx)?;
        let audio_guard = if self.descriptions.audio.is_some() {
            self.install_buffer_probe("audio_monitor", audio_tx)?;
            None
        } else {
            Some(audio_tx)
        };
        let video_codec_guard = if self.pipeline.by_name("video_codec_monitor").is_some() {
            self.install_buffer_probe("video_codec_monitor", video_codec_tx)?;
            None
        } else {
            Some(video_codec_tx)
        };
        let video_ingress_guard = if self.pipeline.by_name("video_ingress_monitor").is_some() {
            self.install_buffer_probe("video_ingress_monitor", video_ingress_tx)?;
            None
        } else {
            Some(video_ingress_tx)
        };
        let audio_ingress_guard = if self.pipeline.by_name("audio_ingress_monitor").is_some() {
            self.install_buffer_probe("audio_ingress_monitor", audio_ingress_tx)?;
            None
        } else {
            Some(audio_ingress_tx)
        };
        let video_egress_guard = if self.pipeline.by_name("video_egress_monitor").is_some() {
            self.install_buffer_probe("video_egress_monitor", video_egress_tx)?;
            None
        } else {
            Some(video_egress_tx)
        };
        let audio_egress_guard = if self.pipeline.by_name("audio_egress_monitor").is_some() {
            self.install_buffer_probe("audio_egress_monitor", audio_egress_tx)?;
            None
        } else {
            Some(audio_egress_tx)
        };

        if let Err(err) = self.pipeline.set_state(gst::State::Playing) {
            let mut detail = None;
            while let Some(message) = bus.timed_pop(gst::ClockTime::ZERO) {
                if let gst::MessageView::Error(error) = message.view() {
                    detail = Some(format!(
                        "error from {}: {}{}",
                        source_name(&message),
                        error.error(),
                        error
                            .debug()
                            .map(|debug| format!("; {debug}"))
                            .unwrap_or_default()
                    ));
                    break;
                }
            }
            if let Some(detail) = detail {
                return Err(anyhow!(
                    "failed to start {} pipeline: {:?}; {}",
                    self.name,
                    err,
                    detail
                ));
            }
            return Err(anyhow!("failed to start {} pipeline: {:?}", self.name, err));
        }

        let stop_flag = Arc::clone(&self.stop_flag);
        let pipeline_name = self.name.to_string();
        let pipeline = self.pipeline.clone();
        let bus_poll_interval_ms = self.bus_poll_interval_ms.max(1);
        let bus_thread = thread::spawn(move || {
            let mut qos_total = 0_u64;
            let mut terminal_sent = false;
            while !stop_flag.load(Ordering::Relaxed) {
                let Some(message) =
                    bus.timed_pop(gst::ClockTime::from_mseconds(bus_poll_interval_ms))
                else {
                    continue;
                };
                let event = match message.view() {
                    gst::MessageView::Error(err) => Some(PipelineEvent::Error(format!(
                        "{} error from {}: {}{}",
                        pipeline_name,
                        source_name(&message),
                        err.error(),
                        err.debug()
                            .map(|debug| format!("; {debug}"))
                            .unwrap_or_default()
                    ))),
                    gst::MessageView::Warning(warn) => {
                        let source = source_name(&message);
                        let text = format!(
                            "{} warning from {}: {}",
                            pipeline_name,
                            source,
                            warn.error()
                        );
                        if text.to_ascii_lowercase().contains("underrun")
                            && is_audio_source_name(&source)
                        {
                            Some(PipelineEvent::AudioUnderrun)
                        } else {
                            Some(PipelineEvent::Warning(text))
                        }
                    }
                    gst::MessageView::Eos(..) => Some(PipelineEvent::Eos),
                    gst::MessageView::ClockLost(..) => Some(PipelineEvent::ClockLost),
                    gst::MessageView::Latency(..) => {
                        if let Err(err) = pipeline.recalculate_latency() {
                            Some(PipelineEvent::Warning(format!(
                                "{} failed to recalculate latency: {}",
                                pipeline_name, err
                            )))
                        } else {
                            Some(PipelineEvent::Latency)
                        }
                    }
                    gst::MessageView::Qos(..) => {
                        qos_total = qos_total.saturating_add(1);
                        qos_tx.send_replace(qos_total);
                        None
                    }
                    gst::MessageView::Element(element) => {
                        if let Some(structure) = element.structure() {
                            let name = structure.name().to_ascii_lowercase();
                            let source = source_name(&message);
                            if (name.contains("underrun") || name.contains("xrun"))
                                && is_audio_source_name(&source)
                            {
                                Some(PipelineEvent::AudioUnderrun)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    _ => None,
                };

                if let Some(event) = event {
                    if event.requires_restart() {
                        if !terminal_sent {
                            terminal_tx.send_replace(Some(event));
                            terminal_sent = true;
                        }
                        continue;
                    }
                    match bus_tx.try_send(event) {
                        Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                        Err(mpsc::error::TrySendError::Closed(_)) => break,
                    }
                }
            }
        });
        self.bus_thread = Some(bus_thread);
        Ok(PipelineEvents {
            bus: bus_rx,
            terminal: terminal_rx,
            video: video_rx,
            audio: audio_rx,
            video_codec: video_codec_rx,
            video_ingress: video_ingress_rx,
            audio_ingress: audio_ingress_rx,
            video_egress: video_egress_rx,
            audio_egress: audio_egress_rx,
            qos: qos_rx,
            _audio_guard: audio_guard,
            _video_codec_guard: video_codec_guard,
            _video_ingress_guard: video_ingress_guard,
            _audio_ingress_guard: audio_ingress_guard,
            _video_egress_guard: video_egress_guard,
            _audio_egress_guard: audio_egress_guard,
        })
    }

    fn install_buffer_probe(
        &self,
        element_name: &str,
        sender: watch::Sender<MediaHeartbeat>,
    ) -> Result<()> {
        let element = self.pipeline.by_name(element_name).ok_or_else(|| {
            anyhow!(
                "{} pipeline element {} is not available",
                self.name,
                element_name
            )
        })?;
        let pad = element.static_pad("src").ok_or_else(|| {
            anyhow!(
                "{} pipeline element {} has no src pad",
                self.name,
                element_name
            )
        })?;
        let total = AtomicU64::new(0);
        let started = Instant::now();
        let notifier = HeartbeatNotifier::new();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            let total = total.fetch_add(1, Ordering::Relaxed).saturating_add(1);
            let observed_at = Instant::now();
            let elapsed_ms = observed_at.duration_since(started).as_millis() as u64;
            if notifier.should_notify(elapsed_ms) {
                sender.send_replace(MediaHeartbeat {
                    observed_at: Some(observed_at),
                    total,
                });
            }
            gst::PadProbeReturn::Ok
        })
        .ok_or_else(|| anyhow!("failed to install buffer probe on {}", element_name))?;
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(bus) = self.pipeline.bus() {
            bus.set_flushing(true);
        }
        if let Some(bus_thread) = self.bus_thread.take() {
            let _ = bus_thread.join();
        }
        self.pipeline
            .set_state(gst::State::Null)
            .map_err(|err| anyhow!("failed to stop {} pipeline: {:?}", self.name, err))?;
        Ok(())
    }

    fn new(
        name: &'static str,
        descriptions: PipelineDescriptions,
        bus_poll_interval_ms: u64,
    ) -> Result<Self> {
        init_gstreamer()?;
        let bin = gst::parse::bin_from_description(&descriptions.full, true)
            .with_context(|| format!("failed to parse {} pipeline", name))?;
        let pipeline = gst::Pipeline::new();
        pipeline
            .add(&bin)
            .with_context(|| format!("failed to assemble {} pipeline", name))?;
        Ok(Self {
            name,
            descriptions,
            pipeline,
            stop_flag: Arc::new(AtomicBool::new(false)),
            bus_thread: None,
            bus_poll_interval_ms,
        })
    }
}

impl Drop for GstServicePipeline {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(bus) = self.pipeline.bus() {
            bus.set_flushing(true);
        }
        if let Some(bus_thread) = self.bus_thread.take() {
            let _ = bus_thread.join();
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

fn init_gstreamer() -> Result<()> {
    gst::init().context("failed to initialize gstreamer")
}

fn build_tx_descriptions(
    config: &TxConfig,
    interface_name: Option<&str>,
    force_video_conversion: bool,
) -> PipelineDescriptions {
    let video = tx_video_branch(config, interface_name, force_video_conversion);
    let audio = config
        .audio
        .enabled
        .then(|| tx_audio_branch(config, interface_name));
    PipelineDescriptions {
        full: join_branches(&video, audio.as_deref()),
        video,
        audio,
        renderer: None,
    }
}

fn build_rx_descriptions(config: &RxConfig, interface_name: Option<&str>) -> PipelineDescriptions {
    let (video, renderer_name) = rx_video_branch(config, interface_name, &config.video.renderer);
    let audio = config
        .audio
        .enabled
        .then(|| rx_audio_branch(config, interface_name));
    PipelineDescriptions {
        full: join_branches(&video, audio.as_deref()),
        video,
        audio,
        renderer: Some(renderer_name),
    }
}

fn tx_video_branch(
    config: &TxConfig,
    interface_name: Option<&str>,
    force_video_conversion: bool,
) -> String {
    let interface_fragment = interface_name
        .map(|name| format!(" multicast-iface={}", quoted(name)))
        .unwrap_or_default();
    let source = if config.video.source_element.trim().is_empty() {
        format!(
            "v4l2src name=video_src device={} io-mode={} do-timestamp=true",
            quoted(&config.video.device),
            config.video.capture_io_mode.gst_value()
        )
    } else {
        config.video.source_element.clone()
    };
    let (encoder, encoder_input_caps) = select_h264_encoder(config);
    // x264enc with byte-stream=true already emits AU-aligned H.264, which
    // rtph264pay accepts directly. Keep h264parse for hardware encoders and
    // custom fragments whose output caps/sequence headers may differ.
    let direct_x264_rtp = encoder.starts_with("x264enc ") && !encoder.contains('!');
    let h264_parser = if direct_x264_rtp {
        ""
    } else {
        "h264parse config-interval=-1 ! "
    };
    // Preserve SPS/PPS insertion at each IDR when bypassing h264parse.
    let pay_config_interval = if direct_x264_rtp { -1 } else { 1 };
    let source_caps = if config.video.source_caps.trim().is_empty() {
        format!(
            "video/x-raw,width={},height={},framerate={}/1",
            config.video.width, config.video.height, config.video.fps
        )
    } else {
        config.video.source_caps.clone()
    };
    let source_decoder = if config.video.source_decoder_element.trim().is_empty() {
        String::new()
    } else {
        format!(" ! {}", config.video.source_decoder_element.trim())
    };
    // jpegdec supports native I420 for 4:2:0 MJPEG, so x264 can consume it
    // directly without an additional full-frame color conversion. Other JPEG
    // subsampling may negotiate a different raw format; the TX supervisor
    // retries with videoconvert on a not-negotiated GStreamer error.
    let direct_mjpeg_i420 = !force_video_conversion
        && config.video.source_decoder_element.trim() == "jpegdec"
        && source_caps.starts_with("image/jpeg")
        && encoder_input_caps == ",format=I420";
    let conversion = if direct_mjpeg_i420 {
        ""
    } else {
        "videoconvert ! "
    };
    format!(
        concat!(
            "{source} ",
            "! {source_caps} ",
            "! queue silent=true leaky=downstream max-size-buffers=1 max-size-bytes=0 max-size-time=0{source_decoder} ",
            "! {conversion}video/x-raw,width={width},height={height},framerate={fps}/1{encoder_input_caps} ",
            "! identity name=video_ingress_monitor silent=true ",
            "! {encoder} ",
            "! {h264_parser}video/x-h264,stream-format=byte-stream,alignment=au ",
            "! identity name=video_monitor silent=true ",
            "! rtph264pay pt={payload_type} config-interval={pay_config_interval} mtu={mtu} ",
            "! identity name=video_egress_monitor silent=true ",
            "! udpsink host={group} port={port} auto-multicast=true ttl-mc={ttl} sync=false async=false{iface}"
        ),
        source = source,
        source_caps = source_caps,
        source_decoder = source_decoder,
        conversion = conversion,
        width = config.video.width,
        height = config.video.height,
        fps = config.video.fps,
        encoder_input_caps = encoder_input_caps,
        encoder = encoder,
        h264_parser = h264_parser,
        payload_type = config.network.video_payload_type,
        pay_config_interval = pay_config_interval,
        mtu = config.network.rtp_mtu,
        group = quoted(&config.network.multicast_group),
        port = config.network.video_port,
        ttl = config.network.ttl,
        iface = interface_fragment,
    )
}

fn select_h264_encoder(config: &TxConfig) -> (String, &'static str) {
    let requested = config.video.encoder_element.trim();

    if requested.is_empty() || requested == "auto" {
        if matches!(
            config.platform.profile.resolve(),
            PlatformProfile::RaspberryPi
        ) && !is_raspberry_pi_5_family()
            && config.video.bitrate_kbps >= 25
            && config.video.bitrate_kbps.is_multiple_of(25)
            && config.video.gop >= 2
            && has_element("v4l2h264enc")
        {
            if let Some(level) = pi_v4l2_h264_level(
                config.video.width,
                config.video.height,
                config.video.fps,
                config.video.bitrate_kbps,
            ) {
                let bitrate_bps = (config.video.bitrate_kbps as u64) * 1_000;
                return (
                    format!(
                        "v4l2h264enc extra-controls=\"controls,repeat_sequence_header=1,video_bitrate={},h264_i_frame_period={}\" ! video/x-h264,level=(string){}",
                        bitrate_bps, config.video.gop, level
                    ),
                    ",format=NV12",
                );
            }
        }

        return (
            format!(
                "x264enc tune=zerolatency speed-preset=ultrafast bitrate={} key-int-max={} bframes=0 aud=true byte-stream=true",
                config.video.bitrate_kbps, config.video.gop
            ),
            ",format=I420",
        );
    }

    let encoder = config.video.encoder_element.clone();
    let encoder_is_x264 = !encoder.contains('!')
        && encoder
            .split_whitespace()
            .next()
            .is_some_and(|name| name == "x264enc");

    if encoder_is_x264 {
        (
            format!(
                "{} bitrate={} key-int-max={} bframes=0 aud=true byte-stream=true",
                encoder, config.video.bitrate_kbps, config.video.gop
            ),
            ",format=I420",
        )
    } else {
        // A custom encoder fragment is complete and may have different property
        // names or raw input formats, so leave both untouched.
        (encoder, "")
    }
}

fn pi_v4l2_h264_level(
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Option<&'static str> {
    const MAX_MACROBLOCKS_PER_FRAME: u64 = 8_192;
    const MAX_MACROBLOCKS_PER_SECOND: u64 = 245_760;
    const MAX_SAFE_BITRATE_KBPS: u32 = 20_000;

    let macroblocks_wide = (width as u64).div_ceil(16);
    let macroblocks_high = (height as u64).div_ceil(16);
    let macroblocks_per_frame = macroblocks_wide.saturating_mul(macroblocks_high);
    let macroblocks_per_second = macroblocks_per_frame.saturating_mul(fps as u64);

    if macroblocks_per_frame > MAX_MACROBLOCKS_PER_FRAME
        || macroblocks_per_second > MAX_MACROBLOCKS_PER_SECOND
        || bitrate_kbps > MAX_SAFE_BITRATE_KBPS
    {
        return None;
    }

    Some(if bitrate_kbps > 10_000 { "4.1" } else { "4" })
}

fn tx_audio_branch(config: &TxConfig, interface_name: Option<&str>) -> String {
    let interface_fragment = interface_name
        .map(|name| format!(" multicast-iface={}", quoted(name)))
        .unwrap_or_default();
    let source = if config.audio.source_element.trim().is_empty() {
        format!(
            "alsasrc name=audio_src device={} buffer-time={} latency-time={} provide-clock=false use-driver-timestamps={}",
            quoted(&config.audio.device),
            config.audio.buffer_time_us,
            config.audio.latency_time_us,
            if config.audio.use_driver_timestamps {
                "true"
            } else {
                "false"
            }
        )
    } else {
        config.audio.source_element.clone()
    };
    format!(
        concat!(
            "{source} ",
            "! queue leaky=downstream max-size-buffers=8 max-size-bytes=0 max-size-time=0 ",
            "! identity name=audio_ingress_monitor silent=true ",
            "! audioconvert ",
            "! audioresample ",
            "! audio/x-raw,format=S16BE,layout=interleaved,rate={sample_rate},channels={channels} ",
            "! identity name=audio_monitor silent=true ",
            "! rtpL16pay pt={payload_type} mtu={mtu} ",
            "! identity name=audio_egress_monitor silent=true ",
            "! udpsink host={group} port={port} auto-multicast=true ttl-mc={ttl} sync=false async=false{iface}"
        ),
        source = source,
        sample_rate = config.audio.sample_rate,
        channels = config.audio.channels,
        payload_type = config.network.audio_payload_type,
        mtu = config.network.rtp_mtu,
        group = quoted(&config.network.multicast_group),
        port = config.network.audio_port,
        ttl = config.network.ttl,
        iface = interface_fragment,
    )
}

fn rx_video_branch(
    config: &RxConfig,
    interface_name: Option<&str>,
    renderer: &RendererKind,
) -> (String, String) {
    let interface_fragment = interface_name
        .map(|name| format!(" multicast-iface={}", quoted(name)))
        .unwrap_or_default();
    let buffer_size_fragment = if config.network.receive_buffer_size > 0 {
        format!(" buffer-size={}", config.network.receive_buffer_size)
    } else {
        String::new()
    };
    let caps = format!(
        "application/x-rtp,media=video,encoding-name=H264,payload={},clock-rate=90000",
        config.network.video_payload_type
    );
    let decoder = select_h264_decoder(config);
    // rtph264depay already negotiates AU-aligned byte-stream H.264, which
    // avdec_h264 accepts directly. Preserve parsing for V4L2, OpenH264,
    // decodebin and compound/custom decoder fragments.
    let direct_avdec_h264 = decoder.split_whitespace().next() == Some("avdec_h264")
        && !decoder.contains('!');
    let h264_parser = if direct_avdec_h264 { "" } else { "h264parse ! " };
    let sink = if config.video.sink_element.trim().is_empty() {
        render_sink(
            renderer,
            &config.platform.profile,
            config.video.fullscreen,
            config.video.sync,
            config.video.max_lateness_ms,
        )
    } else {
        config.video.sink_element.clone()
    };
    let renderer_name = pipeline_shape(&sink)
        .split(" ! ")
        .last()
        .unwrap_or("unknown")
        .to_string();
    // Hardware V4L2 decoding can negotiate a DRM-compatible buffer directly
    // with kmssink. Routing it through the CPU videoconvert element defeats
    // that path and may require a full-frame copy for every decoded frame.
    // Keep software decoding and custom sinks on the conversion path.
    let direct_kms = decoder == "v4l2h264dec"
        && config.video.sink_element.trim().is_empty()
        && sink.starts_with("kmssink ");
    let post_decode = if direct_kms {
        String::new()
    } else {
        format!(
            " ! videoconvert ! video/x-raw,width={},height={},framerate={}/1",
            config.video.width, config.video.height, config.video.fps
        )
    };
    let pipeline = format!(
        concat!(
            "udpsrc address={group} port={port} auto-multicast=true mtu={mtu}{iface}{buffer_size} caps={caps} ",
            "! identity name=video_ingress_monitor silent=true ",
            "! rtpjitterbuffer latency={latency_ms} drop-on-latency=true do-lost=true ",
            "! rtph264depay wait-for-keyframe=true ",
            "! video/x-h264,stream-format=byte-stream,alignment=au ",
            "! {h264_parser}{decoder}{post_decode} ",
            "! identity name=video_codec_monitor silent=true ",
            "! queue leaky=downstream max-size-buffers=1 max-size-bytes=0 max-size-time=0 ",
            "! identity name=video_monitor silent=true ",
            "! {sink}"
        ),
        port = config.network.video_port,
        group = quoted(&config.network.multicast_group),
        iface = interface_fragment,
        buffer_size = buffer_size_fragment,
        caps = quoted(&caps),
        latency_ms = config.video.jitter_latency_ms,
        mtu = config.network.rtp_mtu,
        decoder = decoder,
        h264_parser = h264_parser,
        post_decode = post_decode,
        sink = sink,
    );
    (pipeline, renderer_name)
}

fn rx_audio_branch(config: &RxConfig, interface_name: Option<&str>) -> String {
    let interface_fragment = interface_name
        .map(|name| format!(" multicast-iface={}", quoted(name)))
        .unwrap_or_default();
    let buffer_size_fragment = if config.network.receive_buffer_size > 0 {
        format!(" buffer-size={}", config.network.receive_buffer_size)
    } else {
        String::new()
    };
    let sink = if config.audio.sink_element.trim().is_empty() {
        let max_lateness_ns = (config.audio.late_threshold_ms as u64) * 1_000_000;
        format!(
            "alsasink device={} sync={} async=false provide-clock=false buffer-time={} latency-time={} qos=true max-lateness={}",
            quoted(&config.audio.device),
            if config.audio.sync { "true" } else { "false" },
            config.audio.buffer_time_us,
            config.audio.latency_time_us,
            max_lateness_ns,
        )
    } else {
        config.audio.sink_element.clone()
    };
    let caps = format!(
        "application/x-rtp,media=audio,encoding-name=L16,payload={},clock-rate={},channels={}",
        config.network.audio_payload_type, config.audio.sample_rate, config.audio.channels
    );
    format!(
        concat!(
            "udpsrc address={group} port={port} auto-multicast=true mtu={mtu}{iface}{buffer_size} caps={caps} ",
            "! identity name=audio_ingress_monitor silent=true ",
            "! rtpjitterbuffer latency={latency_ms} drop-on-latency=true do-lost=true ",
            "! rtpL16depay ",
            "! audioconvert ",
            "! audioresample ",
            "! audio/x-raw,format=S16LE,layout=interleaved,rate={sample_rate},channels={channels} ",
            "! queue leaky=downstream max-size-buffers=8 max-size-bytes=0 max-size-time=0 ",
            "! identity name=audio_monitor silent=true ",
            "! {sink}"
        ),
        port = config.network.audio_port,
        group = quoted(&config.network.multicast_group),
        iface = interface_fragment,
        buffer_size = buffer_size_fragment,
        caps = quoted(&caps),
        latency_ms = config.audio.jitter_latency_ms,
        mtu = config.network.rtp_mtu,
        sample_rate = config.audio.sample_rate,
        channels = config.audio.channels,
        sink = sink,
    )
}

fn render_sink(
    renderer: &RendererKind,
    profile: &PlatformProfile,
    fullscreen: bool,
    sync: bool,
    max_lateness_ms: i64,
) -> String {
    let sync_value = if sync { "true" } else { "false" };
    let max_lateness_ns = if max_lateness_ms < 0 {
        -1
    } else {
        max_lateness_ms.saturating_mul(1_000_000)
    };
    match renderer {
        RendererKind::Auto => match profile.resolve() {
            PlatformProfile::RaspberryPi => format!(
                "kmssink sync={} force-modesetting=false qos=true max-lateness={}",
                sync_value, max_lateness_ns
            ),
            PlatformProfile::LinuxPc | PlatformProfile::Auto => render_linux_sink(
                preferred_linux_sink(fullscreen),
                fullscreen,
                sync,
                max_lateness_ms,
            ),
        },
        RendererKind::Sdl => render_linux_sink(LinuxSink::Sdl, fullscreen, sync, max_lateness_ms),
        RendererKind::KmsDrm => format!(
            "kmssink sync={} force-modesetting=false qos=true max-lateness={}",
            sync_value, max_lateness_ns
        ),
    }
}

fn join_branches(video: &str, audio: Option<&str>) -> String {
    match audio {
        Some(audio) => format!("{} {}", video, audio),
        None => video.to_string(),
    }
}

fn quoted(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

fn source_name(message: &gst::Message) -> String {
    message
        .src()
        .map(|src| src.path_string().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn is_audio_source_name(source: &str) -> bool {
    let source = source.to_ascii_lowercase();
    source.contains("audio") || source.contains("alsa")
}

fn select_h264_decoder(config: &RxConfig) -> String {
    let requested = config.video.decoder_element.trim();
    if requested.is_empty() || requested == "auto" {
        return preferred_h264_decoder(&config.platform.profile);
    }
    requested.to_string()
}

/// Avoid libavcodec frame-level threading, which adds decoder output delay.
/// Slice threading keeps parallel decode available for multi-slice H.264.
pub const LOW_LATENCY_AVDEC_H264: &str = "avdec_h264 thread-type=slice";

fn preferred_h264_decoder(profile: &PlatformProfile) -> String {
    match profile.resolve() {
        PlatformProfile::RaspberryPi => {
            let candidates: &[&str] = if is_raspberry_pi_5_family() {
                &["avdec_h264", "openh264dec", "decodebin"]
            } else {
                &["v4l2h264dec", "avdec_h264", "openh264dec", "decodebin"]
            };
            for candidate in candidates {
                if *candidate == "decodebin" || has_element(candidate) {
                    return if *candidate == "avdec_h264" {
                        LOW_LATENCY_AVDEC_H264.to_string()
                    } else {
                        (*candidate).to_string()
                    };
                }
            }
        }
        PlatformProfile::LinuxPc | PlatformProfile::Auto => {
            for candidate in [
                "avdec_h264",
                "openh264dec",
                "vah264dec",
                "vaapih264dec",
                "decodebin",
            ] {
                if candidate == "decodebin" || has_element(candidate) {
                    return if candidate == "avdec_h264" {
                        LOW_LATENCY_AVDEC_H264.to_string()
                    } else {
                        candidate.to_string()
                    };
                }
            }
        }
    }
    "decodebin".to_string()
}

#[derive(Copy, Clone)]
enum LinuxSink {
    Sdl,
    Wayland,
    XImage,
}

fn render_linux_sink(
    sink: LinuxSink,
    fullscreen: bool,
    sync: bool,
    max_lateness_ms: i64,
) -> String {
    let sync_value = if sync { "true" } else { "false" };
    let max_lateness_ns = if max_lateness_ms < 0 {
        -1
    } else {
        max_lateness_ms.saturating_mul(1_000_000)
    };
    match sink {
        LinuxSink::Sdl => format!(
            "sdlvideosink sync={} fullscreen={} qos=true max-lateness={}",
            sync_value,
            if fullscreen { "true" } else { "false" },
            max_lateness_ns
        ),
        LinuxSink::Wayland => format!(
            "waylandsink sync={} fullscreen={} qos=true max-lateness={}",
            sync_value,
            if fullscreen { "true" } else { "false" },
            max_lateness_ns
        ),
        LinuxSink::XImage => format!(
            "ximagesink sync={} qos=true max-lateness={}",
            sync_value, max_lateness_ns
        ),
    }
}

fn preferred_linux_sink(fullscreen: bool) -> LinuxSink {
    let has_wayland = env::var_os("WAYLAND_DISPLAY").is_some();
    let has_x11 = env::var_os("DISPLAY").is_some();

    if has_wayland && has_element("waylandsink") {
        return LinuxSink::Wayland;
    }
    if (has_wayland || has_x11) && has_element("sdlvideosink") {
        return LinuxSink::Sdl;
    }
    if !fullscreen && has_x11 && has_element("ximagesink") {
        return LinuxSink::XImage;
    }

    // Never use autovideosink here: GstAutoDetect deliberately installs a
    // fake sink when no usable display sink exists, which would make media
    // heartbeats look healthy while no picture is actually rendered.
    //
    // Prefer an installed explicit sink even without a detected session so
    // its READY/PLAYING transition fails visibly if the display is unavailable.
    if has_element("sdlvideosink") {
        return LinuxSink::Sdl;
    }
    if has_element("waylandsink") {
        return LinuxSink::Wayland;
    }
    if !fullscreen && has_element("ximagesink") {
        return LinuxSink::XImage;
    }

    // Returning SDL here intentionally makes parsing fail if the plugin is
    // absent, or state change fail if no usable display backend exists.
    // It also prevents silently ignoring fullscreen=true via ximagesink.
    LinuxSink::Sdl
}

fn has_element(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

fn is_raspberry_pi_5_family() -> bool {
    if fs::read("/proc/device-tree/compatible")
        .ok()
        .is_some_and(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .any(|entry| entry == b"brcm,bcm2712")
        })
    {
        return true;
    }

    fs::read("/proc/device-tree/model")
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .is_some_and(|model| {
            let model = model.trim_end_matches('\0');
            model.contains("Raspberry Pi 5")
                || model.contains("Compute Module 5")
                || model.contains("Raspberry Pi 500")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_pipeline_parses(description: &str) {
        init_gstreamer().unwrap();
        gst::parse::bin_from_description(description, true)
            .unwrap_or_else(|err| panic!("pipeline did not parse: {description}: {err}"));
    }

    #[test]
    fn heartbeat_notifier_emits_first_packet_and_coalesces_bursts() {
        let notifier = HeartbeatNotifier::new();
        assert!(notifier.should_notify(0));
        assert!(!notifier.should_notify(0));
        assert!(!notifier.should_notify(24));
        assert!(notifier.should_notify(25));
        assert!(!notifier.should_notify(26));
        assert!(notifier.should_notify(50));
    }

    #[test]
    fn heartbeat_notifier_uses_elapsed_time_not_packet_count() {
        let notifier = HeartbeatNotifier::new();
        assert!(notifier.should_notify(1_000));
        // Even thousands of RTP packets in a short interval need only
        // one wake-up; a later packet refreshes the media watchdog.
        for _ in 0..5_000 {
            assert!(!notifier.should_notify(1_010));
        }
        assert!(notifier.should_notify(1_025));
        assert!(notifier.should_notify(2_000));
    }

    #[test]
    fn default_tx_and_rx_pipelines_parse() {
        GstServicePipeline::for_tx(&TxConfig::default(), None).unwrap();
        GstServicePipeline::for_rx(&RxConfig::default(), None).unwrap();
    }

    #[test]
    fn default_audio_branches_parse() {
        let mut tx = TxConfig::default();
        tx.audio.enabled = true;
        GstServicePipeline::for_tx(&tx, Some("lo"))
            .unwrap_or_else(|err| panic!("default TX audio pipeline did not parse: {err:#}"));

        let mut rx = RxConfig::default();
        rx.audio.enabled = true;
        GstServicePipeline::for_rx(&rx, Some("lo"))
            .unwrap_or_else(|err| panic!("default RX audio pipeline did not parse: {err:#}"));
    }

    #[test]
    fn shipped_pipeline_configs_parse() {
        let config_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs");

        for name in ["tx.default.toml", "tx.pi.toml", "tx.smoketest.toml"] {
            let config = TxConfig::load(config_dir.join(name))
                .unwrap_or_else(|err| panic!("failed to load {name}: {err:#}"));
            GstServicePipeline::for_tx(&config, Some("lo"))
                .unwrap_or_else(|err| panic!("failed to parse {name}: {err:#}"));
        }

        for name in ["rx.default.toml", "rx.pi.toml", "rx.smoketest.toml"] {
            let config = RxConfig::load(config_dir.join(name))
                .unwrap_or_else(|err| panic!("failed to load {name}: {err:#}"));
            GstServicePipeline::for_rx(&config, Some("lo"))
                .unwrap_or_else(|err| panic!("failed to parse {name}: {err:#}"));
        }
    }

    #[test]
    fn pi_v4l2_kms_path_skips_cpu_video_conversion() {
        let mut rx = RxConfig::default();
        rx.platform.profile = PlatformProfile::RaspberryPi;
        rx.video.renderer = RendererKind::KmsDrm;
        rx.video.decoder_element = "v4l2h264dec".to_string();
        let (branch, _) = rx_video_branch(&rx, Some("lo"), &rx.video.renderer);
        assert!(branch.contains("! v4l2h264dec ! identity name=video_codec_monitor"));
        assert!(!branch.contains("! videoconvert"));
        assert!(branch.contains("queue leaky=downstream max-size-buffers=1 "));
        assert!(branch.contains("! kmssink "));
    }

    #[test]
    fn low_latency_avdec_decoder_keeps_slice_threading_and_custom_choices() {
        let mut rx = RxConfig::default();
        rx.platform.profile = PlatformProfile::LinuxPc;
        let automatically_selected = select_h264_decoder(&rx);
        if has_element("avdec_h264") {
            assert_eq!(automatically_selected, LOW_LATENCY_AVDEC_H264);
            let (pipeline, _) = rx_video_branch(&rx, Some("lo"), &rx.video.renderer);
            assert!(pipeline.contains("! avdec_h264 thread-type=slice ! videoconvert"));
            assert_pipeline_parses(&pipeline);
        }

        rx.video.decoder_element = "avdec_h264 max-threads=1".into();
        assert_eq!(select_h264_decoder(&rx), "avdec_h264 max-threads=1");
        rx.video.decoder_element = "openh264dec".into();
        assert_eq!(select_h264_decoder(&rx), "openh264dec");
        rx.video.decoder_element = "v4l2h264dec".into();
        assert_eq!(select_h264_decoder(&rx), "v4l2h264dec");
    }

    #[test]
    fn software_h264_receive_path_skips_parser_but_hardware_keeps_it() {
        let mut rx = RxConfig::default();
        rx.video.decoder_element = LOW_LATENCY_AVDEC_H264.to_string();
        let (software, _) = rx_video_branch(&rx, Some("lo"), &rx.video.renderer);
        assert!(software.contains(
            "! video/x-h264,stream-format=byte-stream,alignment=au ! avdec_h264 thread-type=slice"
        ));
        assert!(!software.contains("! h264parse "));
        assert_pipeline_parses(&software);

        rx.video.decoder_element = "avdec_h264".into();
        let (legacy_software, _) = rx_video_branch(&rx, Some("lo"), &rx.video.renderer);
        assert!(!legacy_software.contains("! h264parse "));
        assert_pipeline_parses(&legacy_software);

        rx.video.decoder_element = "v4l2h264dec".into();
        let (hardware, _) = rx_video_branch(&rx, Some("lo"), &rx.video.renderer);
        assert!(hardware.contains("! h264parse ! v4l2h264dec"));

        rx.video.decoder_element = "avdec_h264 ! identity".into();
        let (custom, _) = rx_video_branch(&rx, Some("lo"), &rx.video.renderer);
        assert!(custom.contains("! h264parse ! avdec_h264 ! identity"));
    }

    #[test]
    fn software_decoder_and_custom_video_sinks_retain_conversion() {
        let mut rx = RxConfig::default();
        rx.video.decoder_element = "avdec_h264".to_string();
        let (software, _) = rx_video_branch(&rx, Some("lo"), &rx.video.renderer);
        assert!(software.contains("! avdec_h264 ! videoconvert ! video/x-raw"));
        rx.video.decoder_element = "v4l2h264dec".to_string();
        rx.video.sink_element = "fakesink sync=false".to_string();
        rx.video.renderer = RendererKind::KmsDrm;
        let (custom_sink, _) = rx_video_branch(&rx, Some("lo"), &rx.video.renderer);
        assert!(custom_sink.contains("! v4l2h264dec ! videoconvert ! video/x-raw"));
    }

    #[test]
    fn mjpeg_i420_uses_direct_encoder_input_without_cpu_conversion() {
        let mut tx = TxConfig::default();
        tx.video.source_caps = "image/jpeg,width=1920,height=1080,framerate=30/1".into();
        tx.video.source_decoder_element = "jpegdec".into();
        tx.video.encoder_element = "x264enc tune=zerolatency speed-preset=ultrafast".into();
        let direct = tx_video_branch(&tx, Some("lo"), false);
        assert!(direct.contains("queue silent=true leaky=downstream max-size-buffers=1 "));
        assert!(direct
            .contains("! jpegdec ! video/x-raw,width=1920,height=1080,framerate=30/1,format=I420"));
        assert!(!direct.contains("! videoconvert "));
        assert_pipeline_parses(&direct);

        let fallback = tx_video_branch(&tx, Some("lo"), true);
        assert!(fallback.contains("! jpegdec ! videoconvert ! video/x-raw"));
        assert_pipeline_parses(&fallback);
    }

    #[test]
    fn x264_direct_rtp_path_skips_redundant_h264_parser() {
        let mut tx = TxConfig::default();
        tx.video.encoder_element = "x264enc tune=zerolatency speed-preset=ultrafast".into();
        let explicit = tx_video_branch(&tx, Some("lo"), false);
        assert!(explicit
            .contains("byte-stream=true ! video/x-h264,stream-format=byte-stream,alignment=au"));
        assert!(!explicit.contains("h264parse"));
        assert!(explicit.contains("rtph264pay pt=96 config-interval=-1"));
        assert_pipeline_parses(&explicit);

        tx.video.encoder_element = "auto".into();
        let automatic = tx_video_branch(&tx, Some("lo"), false);
        assert!(!automatic.contains("h264parse"));
        assert_pipeline_parses(&automatic);

        tx.video.encoder_element = "v4l2h264enc".into();
        let hardware = tx_video_branch(&tx, Some("lo"), false);
        assert!(hardware.contains(
            "! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au"
        ));
        assert!(hardware.contains("rtph264pay pt=96 config-interval=1"));

        tx.video.encoder_element = "identity ! identity".into();
        let custom = tx_video_branch(&tx, Some("lo"), false);
        assert!(custom.contains("! h264parse config-interval=-1 ! video/x-h264"));
    }

    #[test]
    fn v4l2_capture_io_mode_is_explicit_only_for_v4l2_source() {
        use avoverip_common::config::CaptureIoMode;

        let mut tx = TxConfig::default();
        tx.video.capture_io_mode = CaptureIoMode::Mmap;
        let v4l2 = tx_video_branch(&tx, Some("lo"), false);
        assert!(v4l2.starts_with(
            "v4l2src name=video_src device=\"/dev/video0\" io-mode=mmap do-timestamp=true"
        ));
        assert!(v4l2.contains("! queue silent=true leaky=downstream max-size-buffers=1 "));
        assert_pipeline_parses(&v4l2);

        tx.video.capture_io_mode = CaptureIoMode::Auto;
        let fallback = tx_video_branch(&tx, Some("lo"), false);
        assert!(fallback.contains("io-mode=auto"));

        tx.video.source_element = "videotestsrc name=video_src is-live=true".into();
        let custom = tx_video_branch(&tx, Some("lo"), false);
        assert!(!custom.contains("io-mode="));
    }

    #[test]
    fn raw_capture_and_non_i420_encoders_keep_color_converter() {
        let tx = TxConfig::default();
        let raw = tx_video_branch(&tx, Some("lo"), false);
        assert!(raw.contains("! videoconvert ! video/x-raw"));

        let mut hardware = tx;
        hardware.video.source_caps = "image/jpeg,width=1920,height=1080,framerate=30/1".into();
        hardware.video.source_decoder_element = "jpegdec".into();
        hardware.video.encoder_element = "v4l2h264enc".into();
        let branch = tx_video_branch(&hardware, Some("lo"), false);
        assert!(branch.contains("! jpegdec ! videoconvert ! video/x-raw"));
    }

    #[test]
    fn h264_level_4_limits_cover_1080p30_but_not_1080p60() {
        assert_eq!(pi_v4l2_h264_level(1920, 1080, 30, 8_000), Some("4"));
        assert_eq!(pi_v4l2_h264_level(1920, 1080, 60, 8_000), None);
        assert_eq!(pi_v4l2_h264_level(1920, 1080, 30, 25_000), None);
    }

    #[test]
    fn ximagesink_fragment_is_never_given_a_fake_fullscreen_property() {
        let sink = render_linux_sink(LinuxSink::XImage, false, true, 25);
        assert!(sink.starts_with("ximagesink "));
        assert!(!sink.contains("fullscreen="));
    }

    #[test]
    fn pi_v4l2_level_tracks_high_bitrate_guidance() {
        assert_eq!(pi_v4l2_h264_level(1920, 1080, 30, 8_000), Some("4"));
        assert_eq!(pi_v4l2_h264_level(1920, 1080, 30, 15_000), Some("4.1"));
        assert_eq!(pi_v4l2_h264_level(1920, 1080, 60, 8_000), None);
    }

    #[test]
    fn pi_auto_encoder_avoids_v4l2_for_single_frame_gop() {
        let mut tx = TxConfig::default();
        tx.platform.profile = PlatformProfile::RaspberryPi;
        tx.video.gop = 1;
        let (encoder, _) = select_h264_encoder(&tx);
        assert!(encoder.starts_with("x264enc "));
    }

    #[test]
    fn explicit_renderer_and_decoder_choices_are_honored() {
        let sink = render_sink(
            &RendererKind::Sdl,
            &PlatformProfile::LinuxPc,
            false,
            true,
            25,
        );
        assert!(sink.starts_with("sdlvideosink "));

        let mut rx = RxConfig::default();
        rx.video.decoder_element = "decodebin".to_string();
        assert_eq!(select_h264_decoder(&rx), "decodebin");
    }

    #[test]
    fn supported_linux_sink_fragments_parse() {
        init_gstreamer().unwrap();

        let candidates = [
            (LinuxSink::Wayland, "waylandsink"),
            (LinuxSink::Sdl, "sdlvideosink"),
            (LinuxSink::XImage, "ximagesink"),
        ];

        for (sink, factory) in candidates {
            if has_element(factory) {
                let sink = render_linux_sink(sink, true, true, 25);
                assert_pipeline_parses(&format!("videotestsrc ! videoconvert ! {sink}"));
            }
        }
    }

    #[test]
    fn kms_sink_fragment_parses_when_available() {
        init_gstreamer().unwrap();
        if has_element("kmssink") {
            let sink = render_sink(
                &RendererKind::KmsDrm,
                &PlatformProfile::RaspberryPi,
                true,
                true,
                25,
            );
            assert_pipeline_parses(&format!("videotestsrc ! videoconvert ! {sink}"));
        }
    }
}
