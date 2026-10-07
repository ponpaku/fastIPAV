use anyhow::{anyhow, Result};
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
#[command(name = "rx", version, about = "Low-latency AV-over-IP receiver")]
struct Cli {
    #[arg(short, long)]
    config: Option<String>,
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
    #[arg(long)]
    check_config: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let config_path = cli.config.unwrap_or_else(default_config_path);
    let mut config = RxConfig::load(&config_path)?;
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
    if cli.check_config {
        GstServicePipeline::for_rx(&config, None)?;
        println!("rx config and pipeline OK: {}", config_path);
        return Ok(());
    }

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
    state.set_renderer(resolved_renderer.clone()).await;
    state
        .set_jitter_buffer_ms(config.video.jitter_latency_ms)
        .await;
    if config.audio.enabled {
        state
            .set_audio_jitter_buffer_ms(config.audio.jitter_latency_ms)
            .await;
    }
    seed_estimated_metrics(&config, &state).await;
    state.add_note("receiver supervisor enabled").await;
    if matches!(
        config.video.renderer.resolve(&config.platform.profile),
        RendererKind::KmsDrm
    ) {
        state
            .add_note("receiver is configured for KMS/DRM-oriented rendering")
            .await;
    } else {
        state
            .add_note("receiver is configured for desktop renderer selection")
            .await;
    }

    let mut server = spawn_http_server(config.http.socket_addr()?, state.clone()).await?;
    tokio::select! {
        run_result = run_supervisor(config, state.clone()) => {
            server.abort();
            run_result
        }
        server_result = &mut server => {
            match server_result {
                Ok(()) => Err(anyhow!("observability server exited unexpectedly")),
                Err(err) => Err(anyhow!("observability server task failed: {err}")),
            }
        }
    }
}

fn default_config_path() -> String {
    let installed = "/etc/avoverip/rx.toml";
    if std::path::Path::new(installed).is_file() {
        installed.to_string()
    } else {
        "configs/rx.default.toml".to_string()
    }
}

async fn run_supervisor(config: RxConfig, state: SharedServiceState) -> Result<()> {
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let auto_decoder = matches!(config.video.decoder_element.trim(), "" | "auto");
    let mut force_software_decoder = false;

    loop {
        let mut cycle_config = config.clone();
        if auto_decoder && force_software_decoder {
            cycle_config.video.decoder_element = "avdec_h264".to_string();
        }
        let interface_name = match resolve_interface_name(config.network.interface_override()) {
            Ok(interface_name) => interface_name,
            Err(err) => {
                let reason = format!("failed to resolve multicast interface: {err:#}");
                state.set_interface(None).await;
                state.bump_pipeline_restarts().await;
                state
                    .mark_failed(format!("rx startup retry: {reason}"))
                    .await;
                warn!(
                    "rx startup retry scheduled in {} ms: {}",
                    config.recovery.restart_backoff_ms, reason
                );
                tokio::select! {
                    _ = &mut shutdown => {
                        state.mark_stopping("rx shutting down").await;
                        return Ok(());
                    }
                    _ = tokio::time::sleep(Duration::from_millis(config.recovery.restart_backoff_ms)) => {}
                }
                continue;
            }
        };
        state.set_interface(interface_name.clone()).await;

        let mut pipeline = match GstServicePipeline::for_rx(
            &cycle_config,
            interface_name.as_deref(),
        ) {
            Ok(pipeline) => pipeline,
            Err(err) => {
                let reason = format!("failed to construct rx pipeline: {err:#}");
                state.bump_pipeline_restarts().await;
                state
                    .mark_failed(format!("rx startup retry: {reason}"))
                    .await;
                warn!(
                    "rx startup retry scheduled in {} ms: {}",
                    config.recovery.restart_backoff_ms, reason
                );
                tokio::select! {
                    _ = &mut shutdown => {
                        state.mark_stopping("rx shutting down").await;
                        return Ok(());
                    }
                    _ = tokio::time::sleep(Duration::from_millis(config.recovery.restart_backoff_ms)) => {}
                }
                continue;
            }
        };
        let uses_v4l2_decoder = pipeline.descriptions().video.contains("v4l2h264dec");
        state
            .set_pipeline_descriptions(
                pipeline.descriptions().video.clone(),
                pipeline.descriptions().audio.clone(),
            )
            .await;
        if let Some(renderer) = &pipeline.descriptions().renderer {
            state.set_renderer(renderer.clone()).await;
        }

        state
            .mark_waiting("starting_pipeline", "rx pipeline is starting")
            .await;
        let mut events = match pipeline.start() {
            Ok(events) => events,
            Err(err) => {
                let reason = format!("failed to start rx pipeline: {err:#}");
                if auto_decoder && uses_v4l2_decoder {
                    force_software_decoder = true;
                    state
                        .add_note("automatic V4L2 H.264 decoder failed to start; falling back to avdec_h264")
                        .await;
                }
                let _ = pipeline.stop();
                state.bump_pipeline_restarts().await;
                state
                    .mark_failed(format!("rx startup retry: {reason}"))
                    .await;
                warn!(
                    "rx startup retry scheduled in {} ms: {}",
                    config.recovery.restart_backoff_ms, reason
                );
                tokio::select! {
                    _ = &mut shutdown => {
                        state.mark_stopping("rx shutting down").await;
                        return Ok(());
                    }
                    _ = tokio::time::sleep(Duration::from_millis(config.recovery.restart_backoff_ms)) => {}
                }
                continue;
            }
        };
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
        let mut video_total = 0_u64;
        let mut audio_total = 0_u64;
        let mut qos_total = 0_u64;
        let mut video_ready = false;
        let mut audio_ready = !config.audio.enabled;
        let mut service_ready = false;
        let media_timeout = Duration::from_millis(config.recovery.media_timeout_ms);
        let mut watchdog =
            tokio::time::interval(Duration::from_millis(config.recovery.monitor_interval_ms));
        watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
                changed = events.qos.changed() => {
                    if changed.is_err() {
                        break "QoS heartbeat channel closed".to_string();
                    }
                    let current = *events.qos.borrow_and_update();
                    if current > qos_total {
                        state.add_qos_events(current - qos_total).await;
                        qos_total = current;
                    }
                }
                changed = events.terminal.changed() => {
                    if changed.is_err() {
                        break "pipeline terminal event channel closed".to_string();
                    }
                    let event = events.terminal.borrow_and_update().clone();
                    let Some(event) = event else {
                        continue;
                    };
                    let _ = handle_rx_event(&state, &event).await;
                    break event.message();
                }
                event = events.bus.recv() => {
                    let Some(event) = event else {
                        break "pipeline bus event channel closed".to_string();
                    };
                    if handle_rx_event(&state, &event).await {
                        break event.message();
                    }
                }
                changed = events.video.changed() => {
                    if changed.is_err() {
                        break "video heartbeat channel closed".to_string();
                    }
                    let heartbeat = *events.video.borrow_and_update();
                    if let Some(observed_at) = heartbeat.observed_at {
                        last_video_buffer = observed_at;
                    }
                    if heartbeat.total > video_total {
                        state.add_frames_total(heartbeat.total - video_total).await;
                        video_total = heartbeat.total;
                    }
                    if !video_ready && heartbeat.total > 0 {
                        video_ready = true;
                        info!("rx received first video buffer");
                    }
                    if !service_ready && video_ready && audio_ready {
                        service_ready = true;
                        state.mark_ready("rx media is flowing").await;
                    }
                }
                changed = events.audio.changed() => {
                    if changed.is_err() {
                        break "audio heartbeat channel closed".to_string();
                    }
                    let heartbeat = *events.audio.borrow_and_update();
                    if let Some(observed_at) = heartbeat.observed_at {
                        last_audio_buffer = observed_at;
                    }
                    if heartbeat.total > audio_total {
                        state.add_audio_chunks_total(heartbeat.total - audio_total).await;
                        audio_total = heartbeat.total;
                    }
                    if !audio_ready && heartbeat.total > 0 {
                        audio_ready = true;
                        info!("rx received first audio buffer");
                    }
                    if !service_ready && video_ready && audio_ready {
                        service_ready = true;
                        state.mark_ready("rx media is flowing").await;
                    }
                }
            }
        };

        let restart_reason_lower = restart_reason.to_ascii_lowercase();
        if auto_decoder
            && uses_v4l2_decoder
            && ((!video_ready && restart_reason_lower.contains("error"))
                || restart_reason_lower.contains("v4l2h264dec"))
        {
            force_software_decoder = true;
            state
                .add_note("automatic V4L2 H.264 decoder became unusable; falling back to avdec_h264")
                .await;
        }

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
        tokio::select! {
            _ = &mut shutdown => {
                state.mark_stopping("rx shutting down").await;
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_millis(config.recovery.restart_backoff_ms)) => {}
        }
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
        let audio_offset =
            config.audio.jitter_latency_ms as f64 - config.video.jitter_latency_ms as f64;
        state.set_audio_offset(audio_offset).await;
        state.set_av_sync(audio_offset).await;
        if audio_offset.abs() > config.audio.sync_tolerance_ms as f64 {
            state
                .add_note(format!(
                    "configured audio/video jitter offset {:.1} ms exceeds sync tolerance {} ms",
                    audio_offset, config.audio.sync_tolerance_ms
                ))
                .await;
        }
        state
            .add_note("A/V offset is a configuration-based estimate; independent RTP streams are not sender-clock synchronized")
            .await;
    } else {
        state
            .add_note("capture-to-display estimate is seeded from configured video jitter buffer and frame interval")
            .await;
    }
}
