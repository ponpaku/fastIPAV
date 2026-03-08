use anyhow::{Context, Result};
use avoverip_backend_gst::{GstServicePipeline, PipelineEvent};
use avoverip_common::{
    config::TxConfig,
    metrics::SharedServiceState,
    net::resolve_interface_name,
    observability::{init_tracing, spawn_http_server},
};
use clap::Parser;
use std::time::Duration;
use tracing::{error, info, warn};

#[derive(Debug, Parser)]
#[command(name = "tx", about = "Low-latency AV-over-IP transmitter")]
struct Cli {
    #[arg(short, long, default_value = "configs/tx.default.toml")]
    config: String,
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let mut config = TxConfig::load(&cli.config)?;
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

    let interface_name = resolve_interface_name(config.network.interface_override())
        .context("failed to resolve multicast interface")?;
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
    state
        .set_interface(interface_name.clone())
        .await;
    state.set_renderer("not_applicable").await;
    state
        .add_note("phase1/2 transmitter supervisor enabled")
        .await;
    if config.audio.enabled {
        state
            .add_note("audio branch enabled with ALSA -> RTP/L16")
            .await;
    } else {
        state
            .add_note("audio branch disabled; enable it after video path validation if needed")
            .await;
    }

    let server = spawn_http_server(config.http.socket_addr()?, state.clone()).await?;
    let run_result = run_supervisor(config, interface_name, state.clone()).await;
    server.abort();
    run_result
}

async fn run_supervisor(
    config: TxConfig,
    interface_name: Option<String>,
    state: SharedServiceState,
) -> Result<()> {
    loop {
        let mut pipeline = GstServicePipeline::for_tx(&config, interface_name.as_deref())?;
        state
            .set_pipeline_descriptions(
                pipeline.descriptions().video.clone(),
                pipeline.descriptions().audio.clone(),
            )
            .await;
        state.set_state("starting_pipeline").await;
        let mut events = pipeline.start()?;
        state.mark_ready("tx pipeline is running").await;
        info!("tx pipeline launched");
        info!("tx video pipeline: {}", pipeline.descriptions().video);
        if let Some(audio_pipeline) = &pipeline.descriptions().audio {
            info!("tx audio pipeline: {}", audio_pipeline);
        }

        let restart_reason = loop {
            tokio::select! {
                _ = shutdown_signal() => {
                    info!("shutdown requested");
                    state.mark_stopping("tx shutting down").await;
                    if let Err(err) = pipeline.stop() {
                        error!("failed to stop tx pipeline cleanly: {:?}", err);
                    }
                    return Ok(());
                }
                event = events.recv() => {
                    let Some(event) = event else {
                        break "pipeline event channel closed".to_string();
                    };
                    if handle_tx_event(&state, &event).await {
                        break event.message();
                    }
                }
            }
        };

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

async fn handle_tx_event(state: &SharedServiceState, event: &PipelineEvent) -> bool {
    match event {
        PipelineEvent::Info(message) => {
            state.add_note(message.clone()).await;
            false
        }
        PipelineEvent::Warning(message) => {
            state.add_note(message.clone()).await;
            if message.to_ascii_lowercase().contains("dropped") {
                state.bump_dropped_frames().await;
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
