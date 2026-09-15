use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::{FfmpegEvent, LogLevel};
use std::path::Path;

#[derive(Debug, Clone, Copy)]
pub(crate) enum TranscodeTarget {
    H264,
    Vp9,
}

#[derive(Clone, Copy)]
struct VideoEncoder {
    codec: &'static str,
    options: &'static [&'static str],
}

#[cfg(target_os = "macos")]
fn video_encoders(target: TranscodeTarget) -> Vec<VideoEncoder> {
    match target {
        TranscodeTarget::H264 => vec![
            VideoEncoder {
                codec: "h264_videotoolbox",
                options: &["-allow_sw", "1", "-q:v", "65", "-profile:v", "high"],
            },
            VideoEncoder {
                codec: "libx264",
                options: &["-preset", "veryfast", "-crf", "20"],
            },
        ],
        TranscodeTarget::Vp9 => vec![VideoEncoder {
            codec: "libvpx-vp9",
            options: &[
                "-crf",
                "30",
                "-b:v",
                "0",
                "-deadline",
                "good",
                "-cpu-used",
                "4",
            ],
        }],
    }
}

#[cfg(target_os = "windows")]
fn video_encoders(target: TranscodeTarget) -> Vec<VideoEncoder> {
    match target {
        TranscodeTarget::H264 => vec![VideoEncoder {
            codec: "libx264",
            options: &["-preset", "veryfast", "-crf", "20"],
        }],
        TranscodeTarget::Vp9 => vec![VideoEncoder {
            codec: "libvpx-vp9",
            options: &[
                "-crf",
                "30",
                "-b:v",
                "0",
                "-deadline",
                "good",
                "-cpu-used",
                "4",
            ],
        }],
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn video_encoders(target: TranscodeTarget) -> Vec<VideoEncoder> {
    match target {
        TranscodeTarget::H264 => vec![VideoEncoder {
            codec: "libx264",
            options: &["-preset", "veryfast", "-crf", "20"],
        }],
        TranscodeTarget::Vp9 => vec![VideoEncoder {
            codec: "libvpx-vp9",
            options: &[
                "-crf",
                "30",
                "-b:v",
                "0",
                "-deadline",
                "good",
                "-cpu-used",
                "4",
            ],
        }],
    }
}

fn timestamp_seconds(timestamp: &str) -> Option<f64> {
    let mut parts = timestamp.trim().split(':');
    let hours = parts.next()?.parse::<f64>().ok()?;
    let minutes = parts.next()?.parse::<f64>().ok()?;
    let seconds = parts.next()?.parse::<f64>().ok()?;
    (parts.next().is_none()).then_some(hours * 3600.0 + minutes * 60.0 + seconds)
}

fn progress_percent(timestamp: &str, duration_seconds: f64) -> Option<f32> {
    if duration_seconds <= 0.0 {
        return None;
    }
    let current = timestamp_seconds(timestamp)?;
    Some((current / duration_seconds * 100.0).clamp(0.0, 99.9) as f32)
}

struct AttemptFailure {
    message: String,
    cancelled: bool,
}

fn run_ffmpeg(
    ffmpeg_path: &str,
    input: &Path,
    temporary: &Path,
    encoder: VideoEncoder,
    duration_seconds: Option<f64>,
    on_progress: &mut dyn FnMut(f32),
    on_started: &mut dyn FnMut(u32),
    on_finished: &mut dyn FnMut(),
    should_cancel: &dyn Fn() -> bool,
) -> Result<(), AttemptFailure> {
    let mut command = FfmpegCommand::new_with_path(ffmpeg_path);
    command
        .hide_banner()
        .arg("-stats_period")
        .arg("0.2")
        .overwrite()
        .arg("-i")
        .arg(input)
        .args(["-map", "0:v:0", "-map", "0:a:0?", "-map_metadata", "0"])
        .codec_video(encoder.codec)
        .args(encoder.options)
        .pix_fmt("yuv420p")
        .codec_audio("aac")
        .args(["-b:a", "192k", "-movflags", "+faststart"])
        .arg(temporary);

    let mut child = command.spawn().map_err(|error| AttemptFailure {
        message: format!("ffmpegを起動できません: {error}"),
        cancelled: false,
    })?;
    on_started(child.as_inner().id());
    let mut details = Vec::new();
    let mut cancelled = false;
    {
        let events = match child.iter() {
            Ok(events) => events,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                on_finished();
                return Err(AttemptFailure {
                    message: format!("ffmpegの進捗を取得できません: {error}"),
                    cancelled: false,
                });
            }
        };
        for event in events {
            if should_cancel() {
                cancelled = true;
                let _ = child.kill();
                break;
            }
            match event {
                FfmpegEvent::Progress(progress) => {
                    if let Some(duration) = duration_seconds {
                        if let Some(percent) = progress_percent(&progress.time, duration) {
                            on_progress(percent);
                        }
                    }
                }
                FfmpegEvent::Log(LogLevel::Warning | LogLevel::Error | LogLevel::Fatal, line)
                | FfmpegEvent::Error(line) => {
                    if !line.trim().is_empty() {
                        details.push(line.trim().to_string());
                    }
                }
                _ => {}
            }
        }
    }

    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            on_finished();
            return Err(AttemptFailure {
                message: format!("ffmpegの終了を確認できません: {error}"),
                cancelled,
            });
        }
    };
    on_finished();
    if cancelled {
        return Err(AttemptFailure {
            message: "動画変換を中断しました".into(),
            cancelled: true,
        });
    }
    if !status.success() {
        return Err(AttemptFailure {
            message: if details.is_empty() {
                format!("動画変換に失敗しました（終了コード: {:?}）", status.code())
            } else {
                format!("動画変換に失敗しました: {}", details.join("\n"))
            },
            cancelled: false,
        });
    }
    Ok(())
}

pub(crate) fn transcode_mp4(
    ffmpeg_path: &str,
    input: &Path,
    target: TranscodeTarget,
    duration_seconds: Option<f64>,
    on_progress: &mut dyn FnMut(f32),
    on_started: &mut dyn FnMut(u32),
    on_finished: &mut dyn FnMut(),
    should_cancel: &dyn Fn() -> bool,
) -> Result<(), String> {
    if !input.is_file() {
        return Err(format!(
            "変換対象のファイルが見つかりません: {}",
            input.display()
        ));
    }

    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("queuedesk-output");
    let temporary = input.with_file_name(format!("{stem}.queuedesk-convert.tmp.mp4"));
    let _ = std::fs::remove_file(&temporary);
    let encoders = video_encoders(target);
    let mut last_error = None;
    for (index, encoder) in encoders.iter().copied().enumerate() {
        let _ = std::fs::remove_file(&temporary);
        match run_ffmpeg(
            ffmpeg_path,
            input,
            &temporary,
            encoder,
            duration_seconds,
            on_progress,
            on_started,
            on_finished,
            should_cancel,
        ) {
            Ok(()) => {
                last_error = None;
                break;
            }
            Err(error) => {
                let may_fallback = !error.cancelled && index + 1 < encoders.len();
                last_error = Some(error.message);
                if !may_fallback {
                    break;
                }
            }
        }
    }
    if let Some(error) = last_error {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    on_progress(100.0);

    let backup = input.with_file_name(format!("{stem}.queuedesk-original.tmp"));
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(input, &backup).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("変換後ファイルへ置き換えられません: {error}")
    })?;
    if let Err(error) = std::fs::rename(&temporary, input) {
        let _ = std::fs::rename(&backup, input);
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("変換後ファイルへ置き換えられません: {error}"));
    }
    let _ = std::fs::remove_file(backup);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{progress_percent, timestamp_seconds};

    #[test]
    fn parses_ffmpeg_timestamp() {
        assert_eq!(timestamp_seconds("01:02:03.50"), Some(3723.5));
        assert_eq!(timestamp_seconds("not-a-time"), None);
    }

    #[test]
    fn converts_timestamp_to_percent() {
        let percent = progress_percent("00:00:02.50", 10.0).unwrap();
        assert!((percent - 25.0).abs() < 0.01);
    }
}
