use anyhow::{Context, Result};
use avoverip_backend_gst::{GstServicePipeline, PipelineEvent};
use avoverip_common::{
    config::{RendererKind, RxConfig},
    metrics::SharedServiceState,
    net::resolve_interface_name,
    observability::{init_tracing, spawn_http_server},
};
use clap::Parser;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

#[derive(Debug, Parser)]
#[command(name = "rx", about = "Low-latency AV-over-IP receiver")]
struct Cli {
    #[arg(short, long, default_value = "configs/rx.default.toml")]
    config: String,
    #[arg(long)]
    interface: Option<String>,
    #[arg(long)]
    audio_device: Option<String>,
    #[arg(long)]
    enable_audio: bool,
    #[arg(long)]
    bind_addr: Option<String>,
    #[arg(long)]
    http_port: Option<u16>,
    #[arg(long, conflicts_with = "windowed")]
    fullscreen: bool,
    #[arg(long, conflicts_with = "fullscreen")]
    windowed: bool,
    #[arg(long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let mut config = RxConfig::load(&cli.config)?;
    if let Some(interface) = cli.interface {
        config.network.interface = interface;
    }
    if let Some(audio_device) = cli.audio_device {
        config.audio.device = audio_device;
        config.audio.enabled = true;
    }
    if cli.enable_audio {
        config.audio.enabled = true;
    }
    if let Some(bind_addr) = cli.bind_addr {
        config.http.bind_addr = bind_addr;
    }
    if let Some(http_port) = cli.http_port {
        config.http.port = http_port;
    }
    if cli.fullscreen {
        config.video.fullscreen = true;
    }
    if cli.windowed {
        config.video.fullscreen = false;
    }
    config.validate()?;

    let interface_name = resolve_interface_name(config.network.interface_override())
        .context("failed to resolve multicast interface")?;
    let resolved_renderer = config
        .video
        .renderer
        .resolve(&config.platform.profile)
        .as_str()
        .to_string();

    let state = SharedServiceState::new("rx", &config.node_name, "gstreamer");
    state
        .set_network(
            config.network.multicast_group.clone(),
            config.network.video_port,
            config.network.audio_port,
        )
        .await;
    state.set_video_enabled(true).await;
    state.set_audio_enabled(config.audio.enabled).await;
    state.set_interface(interface_name.clone()).await;
    state.set_renderer(resolved_renderer.clone()).await;
    state.set_jitter_buffer_ms(config.video.jitter_latency_ms).await;
    if config.audio.enabled {
        state.set_audio_jitter_buffer_ms(config.audio.jitter_latency_ms).await;
    }
    seed_estimated_metrics(&config, &state).await;
    state
        .add_note("receiver supervisor enabled")
        .await;
    if matches!(config.video.renderer.resolve(&config.platform.profile), RendererKind::KmsDrm) {
        state
            .add_note("receiver is configured for KMS/DRM-oriented rendering")
            .await;
    } else {
        state
            .add_note("receiver is configured for SDL rendering")
            .await;
    }

    let server = spawn_http_server(config.http.socket_addr()?, state.clone()).await?;
    let run_result = run_supervisor(config, state.clone()).await;
    server.abort();
    run_result
}

async fn run_supervisor(
    config: RxConfig,
    state: SharedServiceState,
) -> Result<()> {
    loop {
        let interface_name = resolve_interface_name(config.network.interface_override())
            .context("failed to resolve multicast interface")?;
        state.set_interface(interface_name.clone()).await;
        let mut pipeline = GstServicePipeline::for_rx(&config, interface_name.as_deref())?;
        state
            .set_pipeline_descriptions(
                pipeline.descriptions().video.clone(),
                pipeline.descriptions().audio.clone(),
            )
            .await;
        if let Some(renderer) = &pipeline.descriptions().renderer {
            state.set_renderer(renderer.clone()).await;
        }

        state.set_state("starting_pipeline").await;
        let mut events = pipeline.start()?;
        if config.audio.enabled {
            state
                .mark_waiting("waiting_for_media", "rx waiting for video and audio")
                .await;
            info!("rx pipeline launched; waiting for video and audio buffers");
        } else {
            state
                .mark_waiting("waiting_for_video", "rx waiting for video")
                .await;
            info!("rx pipeline launched; waiting for first video buffer");
        }
        info!("rx video pipeline: {}", pipeline.descriptions().video);
        if let Some(audio_pipeline) = &pipeline.descriptions().audio {
            info!("rx audio pipeline: {}", audio_pipeline);
        }

        let started = Instant::now();
        let mut last_video_buffer = started;
        let mut last_audio_buffer = started;
        let mut last_video_pts_ns: Option<u64> = None;
        let mut last_audio_pts_ns: Option<u64> = None;
        let mut av_sync_out_of_tolerance = false;
        let mut video_ready = false;
        let mut audio_ready = !config.audio.enabled;
        let mut service_ready = false;
        let media_timeout = Duration::from_millis(config.recovery.media_timeout_ms);
        let mut watchdog =
            tokio::time::interval(Duration::from_millis(config.recovery.monitor_interval_ms));
        watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);

        let restart_reason = loop {
            tokio::select! {
                _ = &mut shutdown => {
                    info!("shutdown requested");
                    state.mark_stopping("rx shutting down").await;
                    if let Err(err) = pipeline.stop() {
                        error!("failed to stop rx pipeline cleanly: {:?}", err);
                    }
                    return Ok(());
                }
                _ = watchdog.tick() => {
                    match resolve_interface_name(config.network.interface_override()) {
                        Ok(current) if current != interface_name => {
                            break format!(
                                "multicast interface changed from {:?} to {:?}",
                                interface_name, current
                            );
                        }
                        Err(err) => {
                            break format!("multicast interface unavailable: {}", err);
                        }
                        _ => {}
                    }

                    // A receiver is allowed to start before its transmitter.
                    // Stay unhealthy/waiting until the first media arrives instead
                    // of rebuilding an otherwise valid UDP pipeline repeatedly.
                    if video_ready && last_video_buffer.elapsed() > media_timeout {
                        break format!(
                            "video stream stalled for more than {} ms",
                            config.recovery.media_timeout_ms
                        );
                    }
                    if config.audio.enabled && audio_ready && last_audio_buffer.elapsed() > media_timeout {
                        break format!(
                            "audio stream stalled for more than {} ms",
                            config.recovery.media_timeout_ms
                        );
                    }
                }
                event = events.recv() => {
                    let Some(event) = event else {
                        break "pipeline event channel closed".to_string();
                    };
                    match &event {
                        PipelineEvent::VideoBuffer { pts_ns } => {
                            last_video_buffer = Instant::now();
                            if let Some(pts_ns) = pts_ns {
                                last_video_pts_ns = Some(*pts_ns);
                            }
                            if !video_ready {
                                video_ready = true;
                                info!("rx received first video buffer");
                            }
                        }
                        PipelineEvent::AudioBuffer { pts_ns } => {
                            last_audio_buffer = Instant::now();
                            if let Some(pts_ns) = pts_ns {
                                last_audio_pts_ns = Some(*pts_ns);
                            }
                            if !audio_ready {
                                audio_ready = true;
                                info!("rx received first audio buffer");
                            }
                        }
                        _ => {}
                    }
                    if config.audio.enabled {
                        if let (Some(video_pts_ns), Some(audio_pts_ns)) =
                            (last_video_pts_ns, last_audio_pts_ns)
                        {
                            let offset_ms =
                                (audio_pts_ns as f64 - video_pts_ns as f64) / 1_000_000.0;
                            state.set_audio_offset(offset_ms).await;
                            state.set_av_sync(offset_ms).await;

                            let out_of_tolerance =
                                offset_ms.abs() > config.audio.sync_tolerance_ms as f64;
                            if out_of_tolerance && !av_sync_out_of_tolerance {
                                state
                                    .add_note(format!(
                                        "rx observed A/V timestamp offset {:.1} ms exceeds configured tolerance {} ms",
                                        offset_ms,
                                        config.audio.sync_tolerance_ms
                                    ))
                                    .await;
                            } else if !out_of_tolerance && av_sync_out_of_tolerance {
                                state
                                    .add_note(format!(
                                        "rx observed A/V timestamp offset returned within {} ms tolerance",
                                        config.audio.sync_tolerance_ms
                                    ))
                                    .await;
                            }
                            av_sync_out_of_tolerance = out_of_tolerance;
                        }
                    }
                    if !service_ready && video_ready && audio_ready {
                        service_ready = true;
                        state.mark_ready("rx media is flowing").await;
                    }
                    if handle_rx_event(&state, &event).await {
                        break event.message();
                    }
                }
            }
        };

        if let Err(err) = pipeline.stop() {
            error!("failed to stop rx pipeline before restart: {:?}", err);
        }
        state.bump_pipeline_restarts().await;
        state.set_last_error(restart_reason.clone()).await;
        state
            .mark_failed(format!("rx pipeline restarting: {}", restart_reason))
            .await;
        warn!(
            "rx pipeline restart scheduled in {} ms: {}",
            config.recovery.restart_backoff_ms, restart_reason
        );
        tokio::time::sleep(Duration::from_millis(config.recovery.restart_backoff_ms)).await;
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn handle_rx_event(state: &SharedServiceState, event: &PipelineEvent) -> bool {
    match event {
        PipelineEvent::Info(message) => {
            state.add_note(message.clone()).await;
            false
        }
        PipelineEvent::Warning(message) => {
            let lower = message.to_ascii_lowercase();
            state.add_note(message.clone()).await;
            if lower.contains("late") || lower.contains("dropped") {
                if lower.contains("audio") || lower.contains("alsa") {
                    state.bump_dropped_audio_chunks().await;
                } else {
                    state.bump_dropped_frames().await;
                }
            }
            false
        }
        PipelineEvent::Latency => {
            state
                .add_note("rx pipeline requested latency recalculation")
                .await;
            false
        }
        PipelineEvent::AudioUnderrun => {
            state.bump_audio_underruns().await;
            state
                .add_note("rx detected audio underrun warning from pipeline")
                .await;
            false
        }
        PipelineEvent::VideoBuffer { .. } => {
            state.bump_frames_total().await;
            false
        }
        PipelineEvent::AudioBuffer { .. } => {
            state.bump_audio_chunks_total().await;
            false
        }
        PipelineEvent::Error(message) => {
            state.set_last_error(message.clone()).await;
            true
        }
        PipelineEvent::Eos | PipelineEvent::ClockLost => true,
    }
}

async fn seed_estimated_metrics(config: &RxConfig, state: &SharedServiceState) {
    let frame_interval_ms = 1000.0 / config.video.fps.max(1) as f64;
    let renderer_budget_ms = match config.video.renderer.resolve(&config.platform.profile) {
        RendererKind::KmsDrm => 4.0,
        _ => 8.0,
    };
    let estimate = config.video.jitter_latency_ms as f64 + frame_interval_ms + renderer_budget_ms;
    state.set_latency(estimate).await;
    if config.audio.enabled {
        let audio_offset = config.audio.jitter_latency_ms as f64 - config.video.jitter_latency_ms as f64;
        state.set_audio_offset(audio_offset).await;
        state.set_av_sync(audio_offset).await;
        state
            .add_note("capture-to-display and audio offset are seeded from configured buffers until media timestamps arrive")
            .await;
    } else {
        state
            .add_note("capture-to-display estimate is seeded from configured video jitter buffer and frame interval")
            .await;
    }
}
