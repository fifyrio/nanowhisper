use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Logical height of one overlay row; must match `.overlay-row` in Overlay.css.
pub const ROW_HEIGHT: f64 = 48.0;
/// Vertical gap between rows; must match `.overlay-row:not(:first-child)` margin.
pub const ROW_GAP: f64 = 10.0;

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: u64,
    pub status: String, // "recording" | "transcribing"
}

/// Tracks overlay rows: at most one recording session (the mic is exclusive)
/// plus any number of in-flight transcriptions. Oldest first, newest last —
/// the frontend renders them top to bottom.
pub struct OverlaySessions {
    list: Mutex<Vec<SessionInfo>>,
    recording: Mutex<Option<u64>>,
    next_id: AtomicU64,
}

impl OverlaySessions {
    pub fn new() -> Self {
        Self {
            list: Mutex::new(Vec::new()),
            recording: Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    /// Begin a new recording session, returning its id.
    pub fn start(&self) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.list.lock().unwrap().push(SessionInfo {
            id,
            status: "recording".into(),
        });
        *self.recording.lock().unwrap() = Some(id);
        id
    }

    /// Take the current recording session and flip its row to transcribing.
    pub fn take_recording_as_transcribing(&self) -> Option<u64> {
        let id = self.recording.lock().unwrap().take()?;
        if let Some(s) = self.list.lock().unwrap().iter_mut().find(|s| s.id == id) {
            s.status = "transcribing".into();
        }
        Some(id)
    }

    /// Take the current recording session without keeping its row (cancel).
    pub fn take_recording(&self) -> Option<u64> {
        self.recording.lock().unwrap().take()
    }

    pub fn end(&self, id: u64) {
        self.list.lock().unwrap().retain(|s| s.id != id);
    }

    pub fn snapshot(&self) -> Vec<SessionInfo> {
        self.list.lock().unwrap().clone()
    }
}

pub fn overlay_height(rows: usize) -> f64 {
    rows as f64 * ROW_HEIGHT + rows.saturating_sub(1) as f64 * ROW_GAP
}
