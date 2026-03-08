use anyhow::{anyhow, Context, Result};
use avoverip_common::config::{PlatformProfile, RendererKind, RxConfig, TxConfig};
use gstreamer as gst;
use gst::prelude::*;
use std::{
    env,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
};
use tokio::sync::mpsc;

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

pub struct GstServicePipeline {
    name: &'static str,
    descriptions: PipelineDescriptions,
    pipeline: gst::Pipeline,
    stop_flag: Arc<AtomicBool>,
    bus_thread: Option<thread::JoinHandle<()>>,
}

impl GstServicePipeline {
    pub fn for_tx(config: &TxConfig, interface_name: Option<&str>) -> Result<Self> {
        init_gstreamer()?;
        let descriptions = build_tx_descriptions(config, interface_name);
        Self::new("tx", descriptions)
    }

    pub fn for_rx(config: &RxConfig, interface_name: Option<&str>) -> Result<Self> {
        init_gstreamer()?;
        let descriptions = build_rx_descriptions(config, interface_name);
        Self::new("rx", descriptions)
    }

    pub fn descriptions(&self) -> &PipelineDescriptions {
        &self.descriptions
    }

    pub fn start(&mut self) -> Result<mpsc::UnboundedReceiver<PipelineEvent>> {
        let bus = self
            .pipeline
            .bus()
            .ok_or_else(|| anyhow!("{} pipeline bus is not available", self.name))?;
        self.pipeline
            .set_state(gst::State::Playing)
            .map_err(|err| anyhow!("failed to start {} pipeline: {:?}", self.name, err))?;

        let (tx, rx) = mpsc::unbounded_channel();
        let stop_flag = Arc::clone(&self.stop_flag);
        let pipeline_name = self.name.to_string();
        let bus_thread = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(250)) else {
                    continue;
                };
                let event = match message.view() {
                    gst::MessageView::Error(err) => {
                        let debug = err
                            .debug()
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "no debug details".to_string());
                        Some(PipelineEvent::Error(format!(
                            "{} error from {}: {} ({})",
                            pipeline_name,
                            source_name(&message),
                            err.error(),
                            debug
                        )))
                    }
                    gst::MessageView::Warning(warn) => {
                        let text = format!(
                            "{} warning from {}: {}",
                            pipeline_name,
                            source_name(&message),
                            warn.error()
                        );
                        if text.to_ascii_lowercase().contains("underrun") {
                            Some(PipelineEvent::AudioUnderrun)
                        } else {
                            Some(PipelineEvent::Warning(text))
                        }
                    }
                    gst::MessageView::Eos(..) => Some(PipelineEvent::Eos),
                    gst::MessageView::ClockLost(..) => Some(PipelineEvent::ClockLost),
                    gst::MessageView::Latency(..) => Some(PipelineEvent::Latency),
                    gst::MessageView::Element(element) => {
                        if let Some(structure) = element.structure() {
                            let name = structure.name().to_ascii_lowercase();
                            if name.contains("underrun") || name.contains("xrun") {
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
                    let should_break = event.requires_restart();
                    if tx.send(event).is_err() {
                        break;
                    }
                    if should_break {
                        break;
                    }
                }
            }
        });
        self.bus_thread = Some(bus_thread);
        Ok(rx)
    }

    pub fn stop(&mut self) -> Result<()> {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(bus_thread) = self.bus_thread.take() {
            let _ = bus_thread.join();
        }
        self.pipeline
            .set_state(gst::State::Null)
            .map_err(|err| anyhow!("failed to stop {} pipeline: {:?}", self.name, err))?;
        Ok(())
    }

    fn new(name: &'static str, descriptions: PipelineDescriptions) -> Result<Self> {
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
        })
    }
}

fn init_gstreamer() -> Result<()> {
    gst::init().context("failed to initialize gstreamer")
}

fn build_tx_descriptions(config: &TxConfig, interface_name: Option<&str>) -> PipelineDescriptions {
    let video = tx_video_branch(config, interface_name);
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
    let renderer = config.video.renderer.resolve(&config.platform.profile);
    let video = rx_video_branch(config, interface_name, &renderer);
    let audio = config
        .audio
        .enabled
        .then(|| rx_audio_branch(config, interface_name));
    PipelineDescriptions {
        full: join_branches(&video, audio.as_deref()),
        video,
        audio,
        renderer: Some(renderer.as_str().to_string()),
    }
}

fn tx_video_branch(config: &TxConfig, interface_name: Option<&str>) -> String {
    let interface_fragment = interface_name
        .map(|name| format!(" multicast-iface={}", quoted(name)))
        .unwrap_or_default();
    let source = if config.video.source_element.trim().is_empty() {
        format!(
            "v4l2src name=video_src device={} do-timestamp=true",
            quoted(&config.video.device)
        )
    } else {
        config.video.source_element.clone()
    };
    let encoder = if config.video.encoder_element.trim().is_empty() {
        "x264enc tune=zerolatency speed-preset=ultrafast".to_string()
    } else {
        config.video.encoder_element.clone()
    };
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
    format!(
        concat!(
            "{source} ",
            "! {source_caps}{source_decoder} ",
            "! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 ",
            "! videoconvert ",
            "! video/x-raw,format=I420 ",
            "! {encoder} bitrate={bitrate_kbps} key-int-max={gop} bframes=0 aud=true byte-stream=true sliced-threads=true ",
            "! h264parse config-interval=-1 ",
            "! rtph264pay pt={payload_type} config-interval=1 mtu={mtu} ",
            "! udpsink host={group} port={port} auto-multicast=true ttl-mc={ttl} sync=false async=false{iface}"
        ),
        source = source,
        source_caps = source_caps,
        source_decoder = source_decoder,
        encoder = encoder,
        bitrate_kbps = config.video.bitrate_kbps,
        gop = config.video.gop,
        payload_type = config.network.video_payload_type,
        mtu = config.network.rtp_mtu,
        group = quoted(&config.network.multicast_group),
        port = config.network.video_port,
        ttl = config.network.ttl,
        iface = interface_fragment,
    )
}

fn tx_audio_branch(config: &TxConfig, interface_name: Option<&str>) -> String {
    let interface_fragment = interface_name
        .map(|name| format!(" multicast-iface={}", quoted(name)))
        .unwrap_or_default();
    format!(
        concat!(
            "alsasrc name=audio_src device={device} buffer-time={buffer_time_us} latency-time={latency_time_us} provide-clock=false use-driver-timestamps={use_driver_timestamps} ",
            "! queue leaky=downstream max-size-buffers=8 max-size-bytes=0 max-size-time=0 ",
            "! audioconvert ",
            "! audioresample ",
            "! audio/x-raw,format=S16BE,layout=interleaved,rate={sample_rate},channels={channels} ",
            "! rtpL16pay pt={payload_type} mtu={mtu} ",
            "! udpsink host={group} port={port} auto-multicast=true ttl-mc={ttl} sync=false async=false{iface}"
        ),
        device = quoted(&config.audio.device),
        buffer_time_us = config.audio.buffer_time_us,
        latency_time_us = config.audio.latency_time_us,
        use_driver_timestamps = if config.audio.use_driver_timestamps {
            "true"
        } else {
            "false"
        },
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
) -> String {
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
    format!(
        concat!(
            "udpsrc address=\"0.0.0.0\" port={port} auto-multicast=true multicast-group={group}{iface}{buffer_size} caps={caps} ",
            "! queue max-size-buffers=8 leaky=downstream ",
            "! rtpjitterbuffer latency={latency_ms} drop-on-latency=true do-lost=true ",
            "! rtph264depay ",
            "! h264parse ",
            "! {decoder} ",
            "! videoconvert ",
            "! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 ",
            "! {sink}"
        ),
        port = config.network.video_port,
        group = quoted(&config.network.multicast_group),
        iface = interface_fragment,
        buffer_size = buffer_size_fragment,
        caps = quoted(&caps),
        latency_ms = config.video.jitter_latency_ms,
        decoder = decoder,
        sink = sink,
    )
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
    let caps = format!(
        "application/x-rtp,media=audio,encoding-name=L16,payload={},clock-rate={},channels={}",
        config.network.audio_payload_type,
        config.audio.sample_rate,
        config.audio.channels
    );
    format!(
        concat!(
            "udpsrc address=\"0.0.0.0\" port={port} auto-multicast=true multicast-group={group}{iface}{buffer_size} caps={caps} ",
            "! queue max-size-buffers=8 leaky=downstream ",
            "! rtpjitterbuffer latency={latency_ms} drop-on-latency=true do-lost=true ",
            "! rtpL16depay ",
            "! audioconvert ",
            "! audioresample ",
            "! audio/x-raw,format=S16LE,layout=interleaved,rate={sample_rate},channels={channels} ",
            "! queue leaky=downstream max-size-buffers=8 max-size-bytes=0 max-size-time=0 ",
            "! alsasink device={device} sync={sync} async=false provide-clock=false buffer-time={buffer_time_us} latency-time={latency_time_us}"
        ),
        port = config.network.audio_port,
        group = quoted(&config.network.multicast_group),
        iface = interface_fragment,
        buffer_size = buffer_size_fragment,
        caps = quoted(&caps),
        latency_ms = config.audio.jitter_latency_ms,
        sample_rate = config.audio.sample_rate,
        channels = config.audio.channels,
        device = quoted(&config.audio.device),
        sync = if config.audio.sync { "true" } else { "false" },
        buffer_time_us = config.audio.buffer_time_us,
        latency_time_us = config.audio.latency_time_us,
    )
}

fn render_sink(
    renderer: &RendererKind,
    profile: &PlatformProfile,
    fullscreen: bool,
    sync: bool,
    max_lateness_ms: u32,
) -> String {
    let sync_value = if sync { "true" } else { "false" };
    let max_lateness_ns = (max_lateness_ms as u64) * 1_000_000;
    match renderer.resolve(profile) {
        RendererKind::Sdl => match preferred_linux_sink() {
            LinuxSink::Sdl2 => format!(
                "sdl2sink sync={} fullscreen={} qos=true max-lateness={}",
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
                sync_value,
                max_lateness_ns
            ),
            LinuxSink::AutoVideo => format!(
                "autovideosink sync={} qos=true max-lateness={}",
                sync_value,
                max_lateness_ns
            ),
            LinuxSink::Fake => "fakesink sync=false async=false".to_string(),
        },
        RendererKind::KmsDrm => {
            if has_element("kmssink") {
                format!(
                    "kmssink sync={} force-modesetting={} qos=true max-lateness={}",
                    sync_value,
                    if fullscreen { "true" } else { "false" },
                    max_lateness_ns
                )
            } else {
                // Fall back to the Linux desktop sink path when KMS is unavailable.
                render_sink(&RendererKind::Sdl, &PlatformProfile::LinuxPc, fullscreen, sync, max_lateness_ms)
            }
        }
        RendererKind::Auto => unreachable!("renderer auto is resolved before sink selection"),
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

fn select_h264_decoder(config: &RxConfig) -> String {
    let requested = config.video.decoder_element.trim();
    if requested.is_empty() || requested == "decodebin" || requested == "auto" {
        return preferred_h264_decoder(&config.platform.profile);
    }
    requested.to_string()
}

fn preferred_h264_decoder(profile: &PlatformProfile) -> String {
    match profile {
        PlatformProfile::RaspberryPi => {
            for candidate in ["v4l2h264dec", "avdec_h264", "openh264dec", "decodebin"] {
                if candidate == "decodebin" || has_element(candidate) {
                    return candidate.to_string();
                }
            }
        }
        PlatformProfile::LinuxPc | PlatformProfile::Auto => {
            for candidate in ["vah264dec", "vaapih264dec", "avdec_h264", "openh264dec", "decodebin"] {
                if candidate == "decodebin" || has_element(candidate) {
                    return candidate.to_string();
                }
            }
        }
    }
    "decodebin".to_string()
}

#[derive(Copy, Clone)]
enum LinuxSink {
    Sdl2,
    Wayland,
    XImage,
    AutoVideo,
    Fake,
}

fn preferred_linux_sink() -> LinuxSink {
    if has_element("sdl2sink") {
        return LinuxSink::Sdl2;
    }
    if has_element("waylandsink") && env::var_os("WAYLAND_DISPLAY").is_some() {
        return LinuxSink::Wayland;
    }
    if has_element("ximagesink") && env::var_os("DISPLAY").is_some() {
        return LinuxSink::XImage;
    }
    if has_element("autovideosink") {
        return LinuxSink::AutoVideo;
    }
    LinuxSink::Fake
}

fn has_element(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}
