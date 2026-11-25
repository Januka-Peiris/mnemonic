//! Speech to text for the phone and watch. It runs whisper.cpp on this Mac,
//! so a recording never leaves the owner's own devices.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Whisper's large-v3-turbo, quantized: close to large-v3's accuracy at a
/// third of the size.
pub const WHISPER_MODEL_FILE: &str = "ggml-large-v3-turbo-q5_0.bin";
pub const WHISPER_MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin";

/// Apps opened from the Finder don't get the shell's PATH.
const TOOL_DIRECTORIES: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];

fn find_tool(name: &str) -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    });
    on_path.or_else(|| {
        TOOL_DIRECTORIES
            .iter()
            .map(|directory| Path::new(directory).join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// Turns a recording in any format ffmpeg reads into text.
pub fn transcribe(audio: &[u8], model: &Path) -> Result<String, String> {
    if !model.is_file() {
        return Err(format!(
            "The speech model isn't on the Mac. Download {WHISPER_MODEL_URL} to {}",
            model.display()
        ));
    }
    let ffmpeg = find_tool("ffmpeg")
        .ok_or_else(|| "ffmpeg isn't installed on the Mac (brew install ffmpeg)".to_string())?;
    let whisper = find_tool("whisper-cli").ok_or_else(|| {
        "whisper.cpp isn't installed on the Mac (brew install whisper-cpp)".to_string()
    })?;
    let directory =
        tempfile::tempdir().map_err(|err| format!("Couldn't save the recording: {err}"))?;
    let recording = directory.path().join("recording");
    std::fs::write(&recording, audio)
        .map_err(|err| format!("Couldn't save the recording: {err}"))?;
    // whisper.cpp reads only 16 kHz mono WAV reliably.
    let wav = directory.path().join("recording.wav");
    run(Command::new(&ffmpeg)
        .args(["-nostdin", "-loglevel", "error", "-i"])
        .arg(&recording)
        .args(["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"])
        .arg(&wav))
    .map_err(|err| format!("Couldn't read the recording: {err}"))?;
    // Whisper writes "you" for a silent recording, as when another app,
    // such as the camera, has the microphone.
    let (_, levels) = run(Command::new(&ffmpeg)
        .args(["-nostdin", "-hide_banner", "-i"])
        .arg(&wav)
        .args(["-af", "volumedetect", "-f", "null", "-"]))
    .map_err(|err| format!("Couldn't read the recording: {err}"))?;
    if peak_volume(&levels).is_some_and(|peak| peak < SILENT_PEAK_DB) {
        return Err(
            "Didn't hear anything. Is another app, like the camera, using the microphone?"
                .to_string(),
        );
    }
    let threads = std::thread::available_parallelism()
        .map(|count| count.get().min(8))
        .unwrap_or(4);
    let (output, _) = run(Command::new(whisper)
        .arg("--model")
        .arg(model)
        .arg("--file")
        .arg(&wav)
        .args(["--language", "en", "--no-timestamps", "--no-prints"])
        .args(["--threads", &threads.to_string()]))
    .map_err(|err| format!("Transcribing failed: {err}"))?;
    let text = spoken_text(&output);
    if text.is_empty() {
        Err("Didn't catch that".to_string())
    } else {
        Ok(text)
    }
}

// Only called from the phone server's request threads, never the UI thread.
#[allow(clippy::disallowed_methods)]
/// The command's stdout and stderr when it succeeds.
fn run(command: &mut Command) -> Result<(String, String), String> {
    let output = command.output().map_err(|err| err.to_string())?;
    if output.status.success() {
        Ok((
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("it stopped with an error")
            .trim()
            .to_string())
    }
}

/// Below this, the loudest moment of a recording is silence rather than
/// someone speaking, even quietly into a distant microphone.
const SILENT_PEAK_DB: f64 = -50.0;

/// The `max_volume` that ffmpeg's volumedetect filter reports, in dB.
fn peak_volume(levels: &str) -> Option<f64> {
    levels.lines().find_map(|line| {
        let (_, value) = line.split_once("max_volume:")?;
        value.trim().trim_end_matches("dB").trim().parse().ok()
    })
}

/// Whisper writes a segment per line, and marks silence and noise with
/// bracketed tags such as `[BLANK_AUDIO]` rather than leaving it empty.
fn spoken_text(output: &str) -> String {
    output
        .lines()
        .map(str::trim)
        .filter(|line| {
            let tag = (line.starts_with('[') && line.ends_with(']'))
                || (line.starts_with('(') && line.ends_with(')'));
            !line.is_empty() && !tag
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_peak_volume() {
        let levels = "[Parsed_volumedetect_0 @ 0x6000] mean_volume: -91.0 dB\n\
                      [Parsed_volumedetect_0 @ 0x6000] max_volume: -91.0 dB\n";
        assert_eq!(peak_volume(levels), Some(-91.0));
        assert_eq!(peak_volume("no levels"), None);
    }

    #[test]
    fn silence_is_not_text() {
        assert_eq!(spoken_text("\n [BLANK_AUDIO]\n"), "");
        assert_eq!(
            spoken_text(" Open a PR\n (keyboard clicking)\n and merge it.\n"),
            "Open a PR and merge it."
        );
    }
}
