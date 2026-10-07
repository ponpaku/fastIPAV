use anyhow::{anyhow, Result};
use avoverip_backend_gst::{GstServicePipeline, PipelineEvent};
use avoverip_common::{
    config::{RendererKind, RxConfig},
    metrics::{pipeline_shape, SharedServiceState},
    net::resolve_interface_name_for_rtp,
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

    let configured_renderer = config.video.renderer.as_str().to_string();

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
    state.set_renderer(configured_renderer).await;
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
        let interface_name = match resolve_interface_name_for_rtp(
            config.network.interface_override(),
            config.network.rtp_mtu,
        ) {
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
                if auto_decoder
                    && !force_software_decoder
                    && reason.to_ascii_lowercase().contains("v4l2h264dec")
                {
                    force_software_decoder = true;
                    state
                        .add_note(
                            "automatic v4l2h264dec pipeline failed to construct; retrying with avdec_h264",
                        )
                        .await;
                }
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
                if auto_decoder
                    && uses_v4l2_decoder
                    && reason.to_ascii_lowercase().contains("v4l2h264dec")
                {
                    force_software_decoder = true;
                    state
                        .add_note(
                            "automatic V4L2 H.264 decoder failed to start; falling back to avdec_h264",
                        )
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
        info!(
            "rx video pipeline: {}",
            pipeline_shape(&pipeline.descriptions().video)
        );
        if let Some(audio_pipeline) = &pipeline.descriptions().audio {
            info!("rx audio pipeline: {}", pipeline_shape(audio_pipeline));
        }

        let started = Instant::now();
        let mut last_video_buffer = started;
        let mut last_audio_buffer = started;
        let mut last_video_ingress = started;
        let mut last_video_codec = started;
        let mut video_total = 0_u64;
        let mut audio_total = 0_u64;
        let mut video_codec_total = 0_u64;
        let mut video_ingress_total = 0_u64;
        let mut audio_ingress_total = 0_u64;
        let mut first_video_ingress: Option<Instant> = None;
        let mut first_video_codec: Option<Instant> = None;
        let mut first_audio_ingress: Option<Instant> = None;
        let mut qos_total = 0_u64;
        let mut video_codec_ready = false;
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
                        match resolve_interface_name_for_rtp(
                config.network.interface_override(),
                config.network.rtp_mtu,
            ) {
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

                        // A receiver may start before its transmitter, so no ingress at
                        // all keeps health unready without restart. Once either enabled
                        // media branch is arriving, the other branch should appear within
                        // the configured timeout or the joined sockets/pipeline are rebuilt.
                        if config.audio.enabled {
                            if first_video_ingress.is_none()
                                && first_audio_ingress
                                    .is_some_and(|first_seen| first_seen.elapsed() > media_timeout)
                            {
                                break format!(
                                    "audio RTP is arriving but no video RTP arrived within {} ms",
                                    config.recovery.media_timeout_ms
                                );
                            }
                            if first_audio_ingress.is_none()
                                && first_video_ingress
                                    .is_some_and(|first_seen| first_seen.elapsed() > media_timeout)
                            {
                                break format!(
                                    "video RTP is arriving but no audio RTP arrived within {} ms",
                                    config.recovery.media_timeout_ms
                                );
                            }
                        }

                        // Once video RTP is arriving, distinguish decoder stalls from
                        // downstream render stalls.
                        if !video_codec_ready
                            && first_video_ingress
                                .is_some_and(|first_seen| first_seen.elapsed() > media_timeout)
                        {
                            break format!(
                                "video RTP is arriving but decoder produced no frames within {} ms",
                                config.recovery.media_timeout_ms
                            );
                        }
                        if video_codec_ready
                            && !video_ready
                            && first_video_codec
                                .is_some_and(|first_seen| first_seen.elapsed() > media_timeout)
                        {
                            break format!(
                                "decoded video is flowing but render path produced no frames within {} ms",
                                config.recovery.media_timeout_ms
                            );
                        }
                        if config.audio.enabled
                            && !audio_ready
                            && first_audio_ingress
                                .is_some_and(|first_seen| first_seen.elapsed() > media_timeout)
                        {
                            break format!(
                                "audio RTP is arriving but receive path produced no audio within {} ms",
                                config.recovery.media_timeout_ms
                            );
                        }
                        if video_codec_ready && last_video_codec.elapsed() > media_timeout {
                            if last_video_ingress.elapsed() <= media_timeout {
                                break format!(
                                    "video RTP is flowing but decoder stalled for more than {} ms",
                                    config.recovery.media_timeout_ms
                                );
                            }
                            break format!(
                                "video RTP/decoder stream stalled for more than {} ms",
                                config.recovery.media_timeout_ms
                            );
                        }
                        if video_ready && last_video_buffer.elapsed() > media_timeout {
                            break format!(
                                "decoded video is flowing but render path stalled for more than {} ms",
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
                    changed = events.video_ingress.changed() => {
                        if changed.is_err() {
                            break "video ingress heartbeat channel closed".to_string();
                        }
                        let heartbeat = *events.video_ingress.borrow_and_update();
                        if let Some(observed_at) = heartbeat.observed_at {
                            last_video_ingress = observed_at;
                        }
                        if heartbeat.total > video_ingress_total {
                            if first_video_ingress.is_none() {
                                first_video_ingress = heartbeat.observed_at;
                            }
                            video_ingress_total = heartbeat.total;
                        }
                    }
                    changed = events.video_codec.changed() => {
                        if changed.is_err() {
                            break "video decoder heartbeat channel closed".to_string();
                        }
                        let heartbeat = *events.video_codec.borrow_and_update();
                        if let Some(observed_at) = heartbeat.observed_at {
                            last_video_codec = observed_at;
                        }
                        if heartbeat.total > video_codec_total {
                            if first_video_codec.is_none() {
                                first_video_codec = heartbeat.observed_at;
                            }
                            video_codec_total = heartbeat.total;
                        }
                        if !video_codec_ready && heartbeat.total > 0 {
                            video_codec_ready = true;
                            info!("rx received first decoded video buffer");
                        }
                    }
                    changed = events.audio_ingress.changed() => {
                        if changed.is_err() {
                            break "audio ingress heartbeat channel closed".to_string();
                        }
                        let heartbeat = *events.audio_ingress.borrow_and_update();
                        if heartbeat.total > audio_ingress_total {
                            if first_audio_ingress.is_none() {
                                first_audio_ingress = heartbeat.observed_at;
                            }
                            audio_ingress_total = heartbeat.total;
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
            && (restart_reason_lower.contains("v4l2h264dec")
                || restart_reason_lower.contains("decoder produced no frames")
                || restart_reason_lower.contains("decoder stalled"))
        {
            force_software_decoder = true;
            state
                .add_note(
                    "automatic V4L2 H.264 decoder became unusable; falling back to avdec_h264",
                )
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
    state
        .add_note("capture-to-display latency is not reported until an end-to-end measurement probe exists")
        .await;
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
    }
}
