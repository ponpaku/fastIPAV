use serde::Serialize;
use std::{
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize)]
pub struct HealthSnapshot {
    pub ok: bool,
    pub role: String,
    pub node_name: String,
    pub state: String,
    pub started_at_epoch_secs: u64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsSnapshot {
    pub role: String,
    pub node_name: String,
    pub backend: String,
    pub renderer: Option<String>,
    pub state: String,
    pub interface: Option<String>,
    pub multicast_group: Option<String>,
    pub video_port: Option<u16>,
    pub audio_port: Option<u16>,
    pub video_pipeline: Option<String>,
    pub audio_pipeline: Option<String>,
    pub video_enabled: bool,
    pub audio_enabled: bool,
    pub frames_total: u64,
    pub dropped_frames: u64,
    pub audio_chunks_total: u64,
    pub dropped_audio_chunks: u64,
    pub audio_underruns: u64,
    pub estimated_capture_to_display_ms: Option<f64>,
    pub estimated_av_sync_ms: Option<f64>,
    pub estimated_audio_offset_ms: Option<f64>,
    pub jitter_buffer_ms: Option<u32>,
    pub audio_jitter_buffer_ms: Option<u32>,
    pub pipeline_restarts: u32,
    pub last_error: Option<String>,
    pub notes: Vec<String>,
    pub uptime_secs: u64,
}

#[derive(Clone)]
pub struct SharedServiceState {
    started_at: Instant,
    health: Arc<RwLock<HealthSnapshot>>,
    stats: Arc<RwLock<StatsSnapshot>>,
}

impl SharedServiceState {
    pub fn new(role: &str, node_name: &str, backend: &str) -> Self {
        let started_at_epoch_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            started_at: Instant::now(),
            health: Arc::new(RwLock::new(HealthSnapshot {
                ok: false,
                role: role.to_string(),
                node_name: node_name.to_string(),
                state: "starting".to_string(),
                started_at_epoch_secs,
                message: "starting".to_string(),
            })),
            stats: Arc::new(RwLock::new(StatsSnapshot {
                role: role.to_string(),
                node_name: node_name.to_string(),
                backend: backend.to_string(),
                renderer: None,
                state: "starting".to_string(),
                interface: None,
                multicast_group: None,
                video_port: None,
                audio_port: None,
                video_pipeline: None,
                audio_pipeline: None,
                video_enabled: true,
                audio_enabled: false,
                frames_total: 0,
                dropped_frames: 0,
                audio_chunks_total: 0,
                dropped_audio_chunks: 0,
                audio_underruns: 0,
                estimated_capture_to_display_ms: None,
                estimated_av_sync_ms: None,
                estimated_audio_offset_ms: None,
                jitter_buffer_ms: None,
                audio_jitter_buffer_ms: None,
                pipeline_restarts: 0,
                last_error: None,
                notes: Vec::new(),
                uptime_secs: 0,
            })),
        }
    }

    pub async fn mark_ready(&self, message: impl Into<String>) {
        let message = message.into();
        let mut health = self.health.write().await;
        health.ok = true;
        health.state = "running".to_string();
        health.message = message.clone();
        drop(health);
        let mut stats = self.stats.write().await;
        stats.state = "running".to_string();
        push_note(&mut stats.notes, message);
    }

    pub async fn mark_failed(&self, message: impl Into<String>) {
        let message = message.into();
        let mut health = self.health.write().await;
        health.ok = false;
        health.state = "failed".to_string();
        health.message = message.clone();
        drop(health);
        let mut stats = self.stats.write().await;
        stats.state = "failed".to_string();
        stats.last_error = Some(message.clone());
        push_note(&mut stats.notes, message);
    }

    pub async fn mark_stopping(&self, message: impl Into<String>) {
        let message = message.into();
        let mut health = self.health.write().await;
        health.ok = false;
        health.state = "stopping".to_string();
        health.message = message.clone();
        drop(health);
        let mut stats = self.stats.write().await;
        stats.state = "stopping".to_string();
        push_note(&mut stats.notes, message);
    }

    pub async fn set_state(&self, state: impl Into<String>) {
        let state = state.into();
        self.health.write().await.state = state.clone();
        self.stats.write().await.state = state;
    }

    pub async fn set_interface(&self, interface: Option<String>) {
        self.stats.write().await.interface = interface;
    }

    pub async fn set_network(&self, multicast_group: String, video_port: u16, audio_port: u16) {
        let mut stats = self.stats.write().await;
        stats.multicast_group = Some(multicast_group);
        stats.video_port = Some(video_port);
        stats.audio_port = Some(audio_port);
    }

    pub async fn set_renderer(&self, renderer: impl Into<String>) {
        self.stats.write().await.renderer = Some(renderer.into());
    }

    pub async fn set_jitter_buffer_ms(&self, latency_ms: u32) {
        self.stats.write().await.jitter_buffer_ms = Some(latency_ms);
    }

    pub async fn set_audio_jitter_buffer_ms(&self, latency_ms: u32) {
        self.stats.write().await.audio_jitter_buffer_ms = Some(latency_ms);
    }

    pub async fn set_latency(&self, latency_ms: f64) {
        self.stats.write().await.estimated_capture_to_display_ms = Some(latency_ms);
    }

    pub async fn set_audio_offset(&self, offset_ms: f64) {
        self.stats.write().await.estimated_audio_offset_ms = Some(offset_ms);
    }

    pub async fn set_av_sync(&self, offset_ms: f64) {
        self.stats.write().await.estimated_av_sync_ms = Some(offset_ms);
    }

    pub async fn set_video_enabled(&self, enabled: bool) {
        self.stats.write().await.video_enabled = enabled;
    }

    pub async fn set_audio_enabled(&self, enabled: bool) {
        self.stats.write().await.audio_enabled = enabled;
    }

    pub async fn set_pipeline_descriptions(
        &self,
        video_pipeline: impl Into<String>,
        audio_pipeline: Option<String>,
    ) {
        let mut stats = self.stats.write().await;
        stats.video_pipeline = Some(video_pipeline.into());
        stats.audio_pipeline = audio_pipeline;
    }

    pub async fn bump_frames_total(&self) {
        self.stats.write().await.frames_total += 1;
    }

    pub async fn bump_dropped_frames(&self) {
        self.stats.write().await.dropped_frames += 1;
    }

    pub async fn bump_audio_chunks_total(&self) {
        self.stats.write().await.audio_chunks_total += 1;
    }

    pub async fn bump_dropped_audio_chunks(&self) {
        self.stats.write().await.dropped_audio_chunks += 1;
    }

    pub async fn bump_audio_underruns(&self) {
        self.stats.write().await.audio_underruns += 1;
    }

    pub async fn bump_pipeline_restarts(&self) {
        self.stats.write().await.pipeline_restarts += 1;
    }

    pub async fn set_last_error(&self, message: impl Into<String>) {
        self.stats.write().await.last_error = Some(message.into());
    }

    pub async fn add_note(&self, note: impl Into<String>) {
        let mut stats = self.stats.write().await;
        push_note(&mut stats.notes, note.into());
    }

    pub async fn health_snapshot(&self) -> HealthSnapshot {
        self.health.read().await.clone()
    }

    pub async fn stats_snapshot(&self) -> StatsSnapshot {
        let mut snapshot = self.stats.read().await.clone();
        snapshot.uptime_secs = self.started_at.elapsed().as_secs();
        snapshot
    }
}

fn push_note(notes: &mut Vec<String>, note: String) {
    notes.push(note);
    if notes.len() > 32 {
        let overflow = notes.len() - 32;
        notes.drain(0..overflow);
    }
}
