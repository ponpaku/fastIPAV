use anyhow::{anyhow, Context, Result};
use avoverip_common::config::{PlatformProfile, RendererKind, RxConfig, TxConfig};
use gst::prelude::*;
use gstreamer as gst;
use std::{
    env, fs,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Instant,
};
use tokio::sync::mpsc;

const EVENT_CHANNEL_CAPACITY: usize = 256;

#[derive(Debug, Clone)]
pub enum PipelineEvent {
    Info(String),
    Warning(String),
    Error(String),
    Eos,
    ClockLost,
    Latency,
    AudioUnderrun,
    VideoBuffer { observed_at: Instant },
    AudioBuffer { observed_at: Instant },
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
            Self::VideoBuffer { .. } => "video buffer received".to_string(),
            Self::AudioBuffer { .. } => "audio buffer received".to_string(),
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

pub struct PipelineEvents {
    pub bus: mpsc::UnboundedReceiver<PipelineEvent>,
    pub media: mpsc::Receiver<PipelineEvent>,
}

#[derive(Debug, Copy, Clone)]
enum MediaKind {
    Video,
    Audio,
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
        init_gstreamer()?;
        let descriptions = build_tx_descriptions(config, interface_name);
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
        let (bus_tx, bus_rx) = mpsc::unbounded_channel();
        let (media_tx, media_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        self.install_buffer_probe("video_monitor", MediaKind::Video, media_tx.clone())?;
        if self.descriptions.audio.is_some() {
            self.install_buffer_probe("audio_monitor", MediaKind::Audio, media_tx.clone())?;
        }

        self.pipeline
            .set_state(gst::State::Playing)
            .map_err(|err| anyhow!("failed to start {} pipeline: {:?}", self.name, err))?;

        let stop_flag = Arc::clone(&self.stop_flag);
        let pipeline_name = self.name.to_string();
        let pipeline = self.pipeline.clone();
        let bus_poll_interval_ms = self.bus_poll_interval_ms.max(1);
        let bus_thread = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                let Some(message) =
                    bus.timed_pop(gst::ClockTime::from_mseconds(bus_poll_interval_ms))
                else {
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
                    let should_break = event.requires_restart();
                    if bus_tx.send(event).is_err() {
                        break;
                    }
                    if should_break {
                        break;
                    }
                }
            }
        });
        self.bus_thread = Some(bus_thread);
        Ok(PipelineEvents {
            bus: bus_rx,
            media: media_rx,
        })
    }

    fn install_buffer_probe(
        &self,
        element_name: &str,
        media_kind: MediaKind,
        sender: mpsc::Sender<PipelineEvent>,
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
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            let observed_at = Instant::now();
            let event = match media_kind {
                MediaKind::Video => PipelineEvent::VideoBuffer { observed_at },
                MediaKind::Audio => PipelineEvent::AudioBuffer { observed_at },
            };
            // Buffer events are heartbeats/counters. Never block a GStreamer
            // streaming thread; if the consumer is temporarily behind, dropping
            // a heartbeat is preferable to unbounded memory growth or pipeline
            // backpressure. Critical bus events use their own low-volume channel.
            let _ = sender.try_send(event);
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
    let (video, renderer_name) = rx_video_branch(config, interface_name, &renderer);
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
    let encoder_is_x264 = !encoder.contains('!')
        && encoder
            .split_whitespace()
            .next()
            .is_some_and(|name| name == "x264enc");
    let encoder = if encoder_is_x264 {
        format!(
            "{} bitrate={} key-int-max={} bframes=0 aud=true byte-stream=true",
            encoder, config.video.bitrate_kbps, config.video.gop
        )
    } else {
        // Non-x264 encoders use different property names/units. Treat a custom
        // encoder_element as a complete configured fragment instead of
        // appending x264-only properties that would make the pipeline invalid.
        encoder
    };
    let encoder_input_caps = if encoder_is_x264 {
        " ! video/x-raw,format=I420"
    } else {
        ""
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
            "! videoconvert{encoder_input_caps} ",
            "! {encoder} ",
            "! h264parse config-interval=-1 ",
            "! identity name=video_monitor silent=true ",
            "! rtph264pay pt={payload_type} config-interval=1 mtu={mtu} ",
            "! udpsink host={group} port={port} auto-multicast=true ttl-mc={ttl} sync=false async=false{iface}"
        ),
        source = source,
        source_caps = source_caps,
        source_decoder = source_decoder,
        encoder_input_caps = encoder_input_caps,
        encoder = encoder,
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
            "! audioconvert ",
            "! audioresample ",
            "! audio/x-raw,format=S16BE,layout=interleaved,rate={sample_rate},channels={channels} ",
            "! identity name=audio_monitor silent=true ",
            "! rtpL16pay pt={payload_type} mtu={mtu} ",
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
    let renderer_name = sink
        .rsplit('!')
        .next()
        .unwrap_or(&sink)
        .trim()
        .split_whitespace()
        .next()
        .unwrap_or("unknown")
        .to_string();
    let pipeline = format!(
        concat!(
            "udpsrc address={group} port={port} auto-multicast=true{iface}{buffer_size} caps={caps} ",
            "! rtpjitterbuffer latency={latency_ms} drop-on-latency=true do-lost=true ",
            "! rtph264depay wait-for-keyframe=true ",
            "! h264parse ",
            "! {decoder} ",
            "! videoconvert ",
            "! video/x-raw,width={width},height={height},framerate={fps}/1 ",
            "! queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 ",
            "! identity name=video_monitor silent=true ",
            "! {sink}"
        ),
        port = config.network.video_port,
        group = quoted(&config.network.multicast_group),
        iface = interface_fragment,
        buffer_size = buffer_size_fragment,
        caps = quoted(&caps),
        latency_ms = config.video.jitter_latency_ms,
        decoder = decoder,
        width = config.video.width,
        height = config.video.height,
        fps = config.video.fps,
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
            "udpsrc address={group} port={port} auto-multicast=true{iface}{buffer_size} caps={caps} ",
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
    max_lateness_ms: u32,
) -> String {
    let sync_value = if sync { "true" } else { "false" };
    let max_lateness_ns = (max_lateness_ms as u64) * 1_000_000;
    match renderer.resolve(profile) {
        RendererKind::Sdl => {
            render_linux_sink(preferred_linux_sink(), fullscreen, sync, max_lateness_ms)
        }
        RendererKind::KmsDrm => format!(
            "kmssink sync={} force-modesetting={} qos=true max-lateness={}",
            sync_value,
            if fullscreen { "true" } else { "false" },
            max_lateness_ns
        ),
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

fn is_audio_source_name(source: &str) -> bool {
    let source = source.to_ascii_lowercase();
    source.contains("audio") || source.contains("alsa")
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
            let candidates: &[&str] = if is_raspberry_pi_5_family() {
                &["avdec_h264", "openh264dec", "decodebin"]
            } else {
                &["v4l2h264dec", "avdec_h264", "openh264dec", "decodebin"]
            };
            for candidate in candidates {
                if *candidate == "decodebin" || has_element(candidate) {
                    return (*candidate).to_string();
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
                    return candidate.to_string();
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
    AutoVideo,
}

fn render_linux_sink(
    sink: LinuxSink,
    fullscreen: bool,
    sync: bool,
    max_lateness_ms: u32,
) -> String {
    let sync_value = if sync { "true" } else { "false" };
    let max_lateness_ns = (max_lateness_ms as u64) * 1_000_000;
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
        // autovideosink is a GstBin, not a GstBaseSink, so it does not expose
        // sync/qos/max-lateness itself. Its selected child sink owns those.
        LinuxSink::AutoVideo => "autovideosink".to_string(),
    }
}

fn preferred_linux_sink() -> LinuxSink {
    let has_wayland = env::var_os("WAYLAND_DISPLAY").is_some();
    let has_x11 = env::var_os("DISPLAY").is_some();

    if has_wayland && has_element("waylandsink") {
        return LinuxSink::Wayland;
    }
    if (has_wayland || has_x11) && has_element("sdlvideosink") {
        return LinuxSink::Sdl;
    }
    if has_x11 && has_element("ximagesink") {
        return LinuxSink::XImage;
    }
    // Do not silently fall back to fakesink here. A receiver that cannot
    // render must fail visibly rather than report healthy while discarding video.
    LinuxSink::AutoVideo
}

fn has_element(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

fn is_raspberry_pi_5_family() -> bool {
    fs::read("/proc/device-tree/model")
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .is_some_and(|model| {
            let model = model.trim_end_matches('\0');
            model.contains("Raspberry Pi 5") || model.contains("Compute Module 5")
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
    fn default_tx_and_rx_pipelines_parse() {
        GstServicePipeline::for_tx(&TxConfig::default(), None).unwrap();
        GstServicePipeline::for_rx(&RxConfig::default(), None).unwrap();
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
    fn supported_linux_sink_fragments_parse() {
        init_gstreamer().unwrap();

        let candidates = [
            (LinuxSink::Wayland, "waylandsink"),
            (LinuxSink::Sdl, "sdlvideosink"),
            (LinuxSink::XImage, "ximagesink"),
            (LinuxSink::AutoVideo, "autovideosink"),
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
