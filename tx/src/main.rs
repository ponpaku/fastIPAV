use anyhow::{anyhow, Result};
use avoverip_backend_gst::{GstServicePipeline, PipelineEvent};
use avoverip_common::{
    config::TxConfig,
    metrics::{pipeline_shape, SharedServiceState},
    net::resolve_interface_name_for_rtp,
    observability::{init_tracing, spawn_http_server},
};
use clap::Parser;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

#[derive(Debug, Parser)]
#[command(name = "tx", version, about = "Low-latency AV-over-IP transmitter")]
struct Cli {
    #[arg(short, long)]
    config: Option<String>,
    #[arg(long)]
    interface: Option<String>,
    #[arg(long)]
    device: Option<String>,
    #[arg(long)]
    audio_device: Option<String>,
    #[arg(long)]
    enable_audio: bool,
    #[arg(long)]
    bind_addr: Option<String>,
    #[arg(long)]
    http_port: Option<u16>,
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
    let mut config = TxConfig::load(&config_path)?;
    if let Some(interface) = cli.interface {
        config.network.interface = interface;
    }
    if let Some(device) = cli.device {
        config.video.device = device;
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
    config.validate()?;
    if cli.check_config {
        GstServicePipeline::for_tx(&config, None)?;
        println!("tx config and pipeline OK: {}", config_path);
        return Ok(());
    }

    let state = SharedServiceState::new("tx", &config.node_name, "gstreamer");
    state
        .set_network(
            config.network.multicast_group.clone(),
            config.network.video_port,
            config.network.audio_port,
        )
        .await;
    state.set_video_enabled(true).await;
    state.set_audio_enabled(config.audio.enabled).await;
    state.set_renderer("not_applicable").await;
    state.add_note("transmitter supervisor enabled").await;
    if config.audio.enabled {
        state
            .add_note("audio branch enabled with ALSA -> RTP/L16")
            .await;
    } else {
        state
            .add_note("audio branch disabled; enable it after video path validation if needed")
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
    let installed = "/etc/avoverip/tx.toml";
    if std::path::Path::new(installed).is_file() {
        installed.to_string()
    } else {
        "configs/tx.default.toml".to_string()
    }
}

async fn run_supervisor(config: TxConfig, state: SharedServiceState) -> Result<()> {
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let auto_encoder = matches!(config.video.encoder_element.trim(), "" | "auto");
    let mut force_software_encoder = false;
    // Some UVC capture cards output 4:2:2 or 4:4:4 JPEG rather than I420.
    // Keep the direct I420 fast path until actual caps negotiation disproves it.
    let mut force_video_conversion = false;

    loop {
        let mut cycle_config = config.clone();
        if auto_encoder && force_software_encoder {
            cycle_config.video.encoder_element =
                "x264enc tune=zerolatency speed-preset=ultrafast".to_string();
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
                    .mark_failed(format!("tx startup retry: {reason}"))
                    .await;
                warn!(
                    "tx startup retry scheduled in {} ms: {}",
                    config.recovery.restart_backoff_ms, reason
                );
                tokio::select! {
                    _ = &mut shutdown => {
                        state.mark_stopping("tx shutting down").await;
                        return Ok(());
                    }
                    _ = tokio::time::sleep(Duration::from_millis(config.recovery.restart_backoff_ms)) => {}
                }
                continue;
            }
        };
        state.set_interface(interface_name.clone()).await;

        let pipeline_result = GstServicePipeline::for_tx_with_conversion(
            &cycle_config,
            interface_name.as_deref(),
            force_video_conversion,
        );
        let mut pipeline = match pipeline_result {
            Ok(pipeline) => pipeline,
            Err(err) => {
                let reason = format!("failed to construct tx pipeline: {err:#}");
                if auto_encoder && !force_software_encoder {
                    force_software_encoder = true;
                    state.bump_pipeline_restarts().await;
                    state
                        .mark_failed(format!(
                            "tx auto codec pipeline failed; retrying once with x264: {reason}"
                        ))
                        .await;
                    warn!(
                        "tx automatic codec pipeline failed; retrying with x264: {}",
                        reason
                    );
                    continue;
                }

                state.bump_pipeline_restarts().await;
                state
                    .mark_failed(format!("tx startup retry: {reason}"))
                    .await;
                warn!(
                    "tx pipeline construction retry scheduled in {} ms: {}",
                    config.recovery.restart_backoff_ms, reason
                );
                tokio::select! {
                    _ = &mut shutdown => {
                        state.mark_stopping("tx shutting down").await;
                        return Ok(());
                    }
                    _ = tokio::time::sleep(Duration::from_millis(config.recovery.restart_backoff_ms)) => {}
                }
                continue;
            }
        };
        let uses_v4l2_encoder = pipeline.descriptions().video.contains("v4l2h264enc");
        let uses_direct_mjpeg = pipeline
            .descriptions()
            .video
            .contains("! jpegdec ! video/x-raw");
        state
            .set_pipeline_descriptions(
                pipeline.descriptions().video.clone(),
                pipeline.descriptions().audio.clone(),
            )
            .await;
        state
            .mark_waiting("starting_pipeline", "tx pipeline is starting")
            .await;
        let mut events = match pipeline.start() {
            Ok(events) => events,
            Err(err) => {
                let reason = format!("failed to start tx pipeline: {err:#}");
                if uses_direct_mjpeg && is_negotiation_failure(&reason) {
                    force_video_conversion = true;
                    state
                        .add_note(
                            "MJPEG I420 fast path could not negotiate; retrying with videoconvert",
                        )
                        .await;
                }
                if auto_encoder
                    && uses_v4l2_encoder
                    && reason.to_ascii_lowercase().contains("v4l2h264enc")
                {
                    force_software_encoder = true;
                    state
                        .add_note(
                            "automatic V4L2 H.264 encoder failed to start; falling back to x264",
                        )
                        .await;
                }
                let _ = pipeline.stop();
                state.bump_pipeline_restarts().await;
                state
                    .mark_failed(format!("tx startup retry: {reason}"))
                    .await;
                warn!(
                    "tx startup retry scheduled in {} ms: {}",
                    config.recovery.restart_backoff_ms, reason
                );
                tokio::select! {
                    _ = &mut shutdown => {
                        state.mark_stopping("tx shutting down").await;
                        return Ok(());
                    }
                    _ = tokio::time::sleep(Duration::from_millis(config.recovery.restart_backoff_ms)) => {}
                }
                continue;
            }
        };
        if config.audio.enabled {
            state
                .mark_waiting("waiting_for_media", "tx waiting for video and audio")
                .await;
            info!("tx pipeline launched; waiting for video and audio buffers");
        } else {
            state
                .mark_waiting("waiting_for_video", "tx waiting for video")
                .await;
            info!("tx pipeline launched; waiting for first video buffer");
        }
        info!(
            "tx video pipeline: {}",
            pipeline_shape(&pipeline.descriptions().video)
        );
        if let Some(audio_pipeline) = &pipeline.descriptions().audio {
            info!("tx audio pipeline: {}", pipeline_shape(audio_pipeline));
        }

        let started = Instant::now();
        let mut last_video_buffer = started;
        let mut last_audio_buffer = started;
        let mut last_video_ingress = started;
        let mut last_audio_ingress = started;
        let mut last_video_egress = started;
        let mut last_audio_egress = started;
        let mut first_video_encoded: Option<Instant> = None;
        let mut first_audio_processed: Option<Instant> = None;
        let mut video_total = 0_u64;
        let mut audio_total = 0_u64;
        let mut video_ingress_total = 0_u64;
        let mut audio_ingress_total = 0_u64;
        let mut video_egress_total = 0_u64;
        let mut audio_egress_total = 0_u64;
        let mut first_video_ingress: Option<Instant> = None;
        let mut first_audio_ingress: Option<Instant> = None;
        let mut qos_total = 0_u64;
        let mut video_ready = false;
        let mut audio_ready = !config.audio.enabled;
        let mut video_egress_ready = false;
        let mut audio_egress_ready = !config.audio.enabled;
        let mut service_ready = false;
        let media_timeout = Duration::from_millis(config.recovery.media_timeout_ms);
        let mut watchdog =
            tokio::time::interval(Duration::from_millis(config.recovery.monitor_interval_ms));
        watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let restart_reason = loop {
            tokio::select! {
                    _ = &mut shutdown => {
                        info!("shutdown requested");
                        state.mark_stopping("tx shutting down").await;
                        if let Err(err) = pipeline.stop() {
                            error!("failed to stop tx pipeline cleanly: {:?}", err);
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

                        if !video_ready {
                            if let Some(first_seen) = first_video_ingress {
                                if first_seen.elapsed() > media_timeout {
                                    if last_video_ingress.elapsed() <= media_timeout {
                                        break format!(
                                            "video source is flowing but encoder produced no H264 within {} ms",
                                            config.recovery.media_timeout_ms
                                        );
                                    }
                                    break format!(
                                        "video source stopped before encoder produced H264 for {} ms",
                                        config.recovery.media_timeout_ms
                                    );
                                }
                            } else if started.elapsed() > media_timeout {
                                break format!(
                                    "no video source buffers received within {} ms",
                                    config.recovery.media_timeout_ms
                                );
                            }
                        }
                        if video_ready
                            && !video_egress_ready
                            && first_video_encoded
                                .is_some_and(|first_seen| first_seen.elapsed() > media_timeout)
                        {
                            break format!(
                                "encoded video is flowing but RTP packetizer produced no packets within {} ms",
                                config.recovery.media_timeout_ms
                            );
                        }
                        if config.audio.enabled && !audio_ready {
                            if let Some(first_seen) = first_audio_ingress {
                                if first_seen.elapsed() > media_timeout {
                                    if last_audio_ingress.elapsed() <= media_timeout {
                                        break format!(
                                            "audio source is flowing but processing produced no RTP-ready audio within {} ms",
                                            config.recovery.media_timeout_ms
                                        );
                                    }
                                    break format!(
                                        "audio source stopped before processing produced output for {} ms",
                                        config.recovery.media_timeout_ms
                                    );
                                }
                            } else if started.elapsed() > media_timeout {
                                break format!(
                                    "no audio source buffers received within {} ms",
                                    config.recovery.media_timeout_ms
                                );
                            }
                        }
                        if config.audio.enabled
                            && audio_ready
                            && !audio_egress_ready
                            && first_audio_processed
                                .is_some_and(|first_seen| first_seen.elapsed() > media_timeout)
                        {
                            break format!(
                                "processed audio is flowing but RTP packetizer produced no packets within {} ms",
                                config.recovery.media_timeout_ms
                            );
                        }
                        if video_ready && last_video_buffer.elapsed() > media_timeout {
                            if last_video_ingress.elapsed() <= media_timeout {
                                break format!(
                                    "video source is flowing but encoder stalled for more than {} ms",
                                    config.recovery.media_timeout_ms
                                );
                            }
                            break format!(
                                "video source/encoder stream stalled for more than {} ms",
                                config.recovery.media_timeout_ms
                            );
                        }
                        if video_egress_ready && last_video_egress.elapsed() > media_timeout {
                            break format!(
                                "video RTP egress stalled for more than {} ms",
                                config.recovery.media_timeout_ms
                            );
                        }
                        if config.audio.enabled && audio_ready && last_audio_buffer.elapsed() > media_timeout {
                            if last_audio_ingress.elapsed() <= media_timeout {
                                break format!(
                                    "audio source is flowing but processing stalled for more than {} ms",
                                    config.recovery.media_timeout_ms
                                );
                            }
                            break format!(
                                "audio source/processing stream stalled for more than {} ms",
                                config.recovery.media_timeout_ms
                            );
                        }
                        if config.audio.enabled
                            && audio_egress_ready
                            && last_audio_egress.elapsed() > media_timeout
                        {
                            break format!(
                                "audio RTP egress stalled for more than {} ms",
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
                        let _ = handle_tx_event(&state, &event).await;
                        break event.message();
                    }
                    event = events.bus.recv() => {
                        let Some(event) = event else {
                            break "pipeline bus event channel closed".to_string();
                        };
                        if handle_tx_event(&state, &event).await {
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
                    changed = events.audio_ingress.changed() => {
                        if changed.is_err() {
                            break "audio ingress heartbeat channel closed".to_string();
                        }
                        let heartbeat = *events.audio_ingress.borrow_and_update();
                        if let Some(observed_at) = heartbeat.observed_at {
                            last_audio_ingress = observed_at;
                        }
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
                            first_video_encoded = heartbeat.observed_at;
                            info!("tx received first encoded video buffer");
                        }
                        if !service_ready
                            && video_ready
                            && video_egress_ready
                            && audio_ready
                            && audio_egress_ready
                        {
                            service_ready = true;
                            state.mark_ready("tx RTP media is flowing").await;
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
                            first_audio_processed = heartbeat.observed_at;
                            info!("tx received first processed audio buffer");
                        }
                        if !service_ready
                            && video_ready
                            && video_egress_ready
                            && audio_ready
                            && audio_egress_ready
                        {
                            service_ready = true;
                            state.mark_ready("tx RTP media is flowing").await;
                        }
                    }
                    changed = events.video_egress.changed() => {
                        if changed.is_err() {
                            break "video RTP egress heartbeat channel closed".to_string();
                        }
                        let heartbeat = *events.video_egress.borrow_and_update();
                        if let Some(observed_at) = heartbeat.observed_at {
                            last_video_egress = observed_at;
                        }
                        if heartbeat.total > video_egress_total {
                            video_egress_total = heartbeat.total;
                        }
                        if !video_egress_ready && heartbeat.total > 0 {
                            video_egress_ready = true;
                            info!("tx emitted first video RTP packet");
                        }
                        if !service_ready
                            && video_ready
                            && video_egress_ready
                            && audio_ready
                            && audio_egress_ready
                        {
                            service_ready = true;
                            state.mark_ready("tx RTP media is flowing").await;
                        }
                    }
                    changed = events.audio_egress.changed() => {
                        if changed.is_err() {
                            break "audio RTP egress heartbeat channel closed".to_string();
                        }
                        let heartbeat = *events.audio_egress.borrow_and_update();
                        if let Some(observed_at) = heartbeat.observed_at {
                            last_audio_egress = observed_at;
                        }
                        if heartbeat.total > audio_egress_total {
                            audio_egress_total = heartbeat.total;
                        }
                        if !audio_egress_ready && heartbeat.total > 0 {
                            audio_egress_ready = true;
                            info!("tx emitted first audio RTP packet");
                        }
                        if !service_ready
                            && video_ready
                            && video_egress_ready
                            && audio_ready
                            && audio_egress_ready
                        {
                            service_ready = true;
                            state.mark_ready("tx RTP media is flowing").await;
                        }
                    }
                }
        };

        let restart_reason_lower = restart_reason.to_ascii_lowercase();
        if uses_direct_mjpeg && is_negotiation_failure(&restart_reason) {
            force_video_conversion = true;
            state
                .add_note(
                    "MJPEG I420 fast path could not negotiate; falling back to videoconvert",
                )
                .await;
        }
        if auto_encoder
            && uses_v4l2_encoder
            && ((!video_ready && restart_reason_lower.contains("encoder produced no h264"))
                || restart_reason_lower.contains("encoder stalled")
                || restart_reason_lower.contains("v4l2h264enc"))
        {
            force_software_encoder = true;
            state
                .add_note("automatic V4L2 H.264 encoder became unusable; falling back to x264")
                .await;
        }

        if let Err(err) = pipeline.stop() {
            error!("failed to stop tx pipeline before restart: {:?}", err);
        }
        state.bump_pipeline_restarts().await;
        state.set_last_error(restart_reason.clone()).await;
        state
            .mark_failed(format!("tx pipeline restarting: {}", restart_reason))
            .await;
        warn!(
            "tx pipeline restart scheduled in {} ms: {}",
            config.recovery.restart_backoff_ms, restart_reason
        );
        tokio::select! {
            _ = &mut shutdown => {
                state.mark_stopping("tx shutting down").await;
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_millis(config.recovery.restart_backoff_ms)) => {}
        }
    }
}

fn is_negotiation_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("not-negotiated") || lower.contains("not negotiated")
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

async fn handle_tx_event(state: &SharedServiceState, event: &PipelineEvent) -> bool {
    match event {
        PipelineEvent::Info(message) => {
            state.add_note(message.clone()).await;
            false
        }
        PipelineEvent::Warning(message) => {
            let lower = message.to_ascii_lowercase();
            state.add_note(message.clone()).await;
            if lower.contains("dropped") {
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
                .add_note("tx pipeline requested latency recalculation")
                .await;
            false
        }
        PipelineEvent::AudioUnderrun => {
            state.bump_audio_underruns().await;
            state
                .add_note("tx detected audio underrun warning from pipeline")
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
