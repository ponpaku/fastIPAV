use anyhow::{Context, Result};
use avoverip_backend_gst::{GstServicePipeline, PipelineEvent};
use avoverip_common::{
    config::{RendererKind, RxConfig},
    metrics::SharedServiceState,
    net::resolve_interface_name,
    observability::{init_tracing, spawn_http_server},
};
use clap::Parser;
use std::time::Duration;
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
        .add_note("phase1/2 receiver supervisor enabled")
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
    let run_result = run_supervisor(config, interface_name, state.clone()).await;
    server.abort();
    run_result
}

async fn run_supervisor(
    config: RxConfig,
    interface_name: Option<String>,
    state: SharedServiceState,
) -> Result<()> {
    loop {
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
        state.mark_ready("rx pipeline is running").await;
        info!("rx pipeline launched");
        info!("rx video pipeline: {}", pipeline.descriptions().video);
        if let Some(audio_pipeline) = &pipeline.descriptions().audio {
            info!("rx audio pipeline: {}", audio_pipeline);
        }

        let restart_reason = loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("shutdown requested");
                    state.mark_stopping("rx shutting down").await;
                    if let Err(err) = pipeline.stop() {
                        error!("failed to stop rx pipeline cleanly: {:?}", err);
                    }
                    return Ok(());
                }
                event = events.recv() => {
                    let Some(event) = event else {
                        break "pipeline event channel closed".to_string();
                    };
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
                state.bump_dropped_frames().await;
            }
            if lower.contains("audio") && lower.contains("drop") {
                state.bump_dropped_audio_chunks().await;
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
        let audio_offset = config.audio.jitter_latency_ms as f64 - config.video.jitter_latency_ms as f64;
        state.set_audio_offset(audio_offset).await;
        state.set_av_sync(audio_offset).await;
        state
            .add_note("capture-to-display and audio offset are seeded from configured buffers until measurement probes are wired")
            .await;
    } else {
        state
            .add_note("capture-to-display estimate is seeded from configured video jitter buffer and frame interval")
            .await;
    }
}
