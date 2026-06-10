//! Keeps the macOS system microphone input gain at a usable level for recording.
//!
//! Other apps (notably WeChat) grab the mic, lower the system input gain, and
//! never restore it — leaving NanoWhisper's waveform tiny and hurting
//! transcription accuracy. We rescue the gain at the start of every recording
//! so the captured level is consistent regardless of what touched the mic last.

/// Read the current system input volume (0–100) via osascript.
#[cfg(target_os = "macos")]
fn current_input_gain() -> Option<u8> {
    let output = std::process::Command::new("osascript")
        .args(["-e", "input volume of (get volume settings)"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Set the system input volume (0–100) via osascript.
#[cfg(target_os = "macos")]
fn set_input_gain(value: u8) {
    let _ = std::process::Command::new("osascript")
        .args(["-e", &format!("set volume input volume {value}")])
        .output();
}

/// If the system mic input gain is below `target` (0–100), raise it to `target`.
/// A `target` of 0 disables the feature. No-op on non-macOS platforms.
///
/// We only ever raise the gain, never lower it, so a user who deliberately set
/// a high level is left alone.
pub fn ensure_min_gain(target: u8) {
    if target == 0 {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        match current_input_gain() {
            Some(current) if current < target => {
                log::info!("Mic input gain {current} below {target}, raising it");
                set_input_gain(target);
            }
            Some(current) => log::debug!("Mic input gain {current} ok (>= {target})"),
            None => log::warn!("Could not read system mic input gain"),
        }
    }
}
