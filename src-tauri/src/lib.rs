use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};

mod transcode;

use transcode::{transcode_mp4, TranscodeTarget};

const STATE_FILE: &str = "state.json";
const SETTINGS_FILE: &str = "settings.json";
const ARCHIVE_FILE: &str = "download-archive.txt";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum DownloadStatus {
    Queued,
    Downloading,
    Paused,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Theme {
    Auto,
    Light,
    Dark,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum LegacyMp4Compatibility {
    #[serde(alias = "native_preferred")]
    H264Preferred,
    #[serde(alias = "transcode_maximum")]
    H264Only,
    H265Vp9Preferred,
    H265Vp9Only,
}

impl Default for LegacyMp4Compatibility {
    fn default() -> Self {
        Self::H264Preferred
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum QualityMode {
    QualityFirst,
    CompatibilityFirst,
}

impl Default for QualityMode {
    fn default() -> Self {
        Self::QualityFirst
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum OutputCodec {
    Auto,
    Vp9,
    H264,
}

impl Default for OutputCodec {
    fn default() -> Self {
        Self::Auto
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DownloadItem {
    id: String,
    url: String,
    title: String,
    progress: f32,
    speed: String,
    eta: String,
    status: DownloadStatus,
    #[serde(default)]
    phase: String,
    #[serde(default)]
    phase_progress: f32,
    error: Option<String>,
    #[serde(default)]
    error_log: Vec<String>,
    output_dir: String,
    profile: String,
    #[serde(default)]
    profile_args: Vec<String>,
    #[serde(default)]
    quality_mode: QualityMode,
    #[serde(default)]
    output_codec: OutputCodec,
    #[serde(default)]
    transcode_if_needed: bool,
    #[serde(default, rename = "mp4_compatibility", skip_serializing)]
    legacy_mp4_compatibility: Option<LegacyMp4Compatibility>,
    #[serde(default)]
    force_redownload: bool,
    #[serde(default)]
    output_path: Option<String>,
    #[serde(default)]
    created_at: u64,
    #[serde(default)]
    updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Settings {
    max_concurrent_downloads: usize,
    default_folder: String,
    default_profile: String,
    resume_on_launch: bool,
    profile_args: Vec<String>,
    #[serde(default)]
    theme: Theme,
    #[serde(default)]
    quality_mode: QualityMode,
    #[serde(default)]
    output_codec: OutputCodec,
    #[serde(default)]
    transcode_if_needed: bool,
    #[serde(default, rename = "mp4_compatibility", skip_serializing)]
    legacy_mp4_compatibility: Option<LegacyMp4Compatibility>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_concurrent_downloads: 3,
            default_folder: "~/Downloads/yt-dlp".into(),
            default_profile: "Default".into(),
            resume_on_launch: true,
            profile_args: vec![
                "-f".into(),
                "bv*+ba/b".into(),
                "--merge-output-format".into(),
                "mp4".into(),
                "--embed-metadata".into(),
                "--embed-thumbnail".into(),
            ],
            theme: Theme::Auto,
            quality_mode: QualityMode::QualityFirst,
            output_codec: OutputCodec::H264,
            transcode_if_needed: true,
            legacy_mp4_compatibility: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppState {
    settings: Settings,
    items: Vec<DownloadItem>,
    yt_dlp_path: Option<String>,
    ffmpeg_path: Option<String>,
    #[serde(default)]
    deno_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StoredState {
    #[serde(default)]
    items: Vec<DownloadItem>,
}

struct Runtime {
    state: AppState,
    processes: HashMap<String, Arc<Mutex<Child>>>,
    ffmpeg_processes: HashMap<String, u32>,
    cancelled: std::collections::HashSet<String>,
    paths: PathBuf,
    last_persist: Instant,
}

type SharedRuntime = Arc<Mutex<Runtime>>;

fn app_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn load_runtime(app: &AppHandle) -> Result<Runtime, String> {
    let dir = app_dir(app)?;
    let state_file = dir.join(STATE_FILE);
    let settings_file = dir.join(SETTINGS_FILE);
    let stored_json = if state_file.exists() {
        Some(std::fs::read_to_string(&state_file).map_err(|e| e.to_string())?)
    } else {
        None
    };
    let legacy_state = stored_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<AppState>(json).ok());
    let settings = if settings_file.exists() {
        let json = std::fs::read_to_string(&settings_file).map_err(|e| e.to_string())?;
        serde_json::from_str::<Settings>(&json).unwrap_or_else(|_| {
            legacy_state
                .as_ref()
                .map(|state| state.settings.clone())
                .unwrap_or_default()
        })
    } else {
        legacy_state
            .as_ref()
            .map(|state| state.settings.clone())
            .unwrap_or_default()
    };
    let items = stored_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<StoredState>(json).ok())
        .map(|state| state.items)
        .or_else(|| legacy_state.map(|state| state.items))
        .unwrap_or_default();
    let mut state = default_state();
    state.settings = settings;
    state.items = items;
    normalize_settings(&mut state.settings);

    let migrated_at = now_unix();
    for item in &mut state.items {
        normalize_item(item);
        if item.profile_args.is_empty() {
            item.profile_args = state.settings.profile_args.clone();
        }
        if item.created_at == 0 {
            item.created_at = migrated_at;
        }
        if item.updated_at == 0 {
            item.updated_at = item.created_at;
        }
        if item.status == DownloadStatus::Downloading {
            item.phase.clear();
            item.phase_progress = 0.0;
            item.status = DownloadStatus::Interrupted;
            item.error = Some("アプリ終了時に中断されました".into());
            item.error_log.push("アプリ終了時に中断されました".into());
            mark_updated(item);
        }
    }
    state.yt_dlp_path = find_executable("yt-dlp");
    state.ffmpeg_path = find_executable("ffmpeg");
    state.deno_path = find_executable("deno");
    let runtime = Runtime {
        state,
        processes: HashMap::new(),
        ffmpeg_processes: HashMap::new(),
        cancelled: std::collections::HashSet::new(),
        paths: dir,
        last_persist: Instant::now(),
    };
    persist(&runtime)?;
    Ok(runtime)
}

fn default_state() -> AppState {
    AppState {
        settings: Settings::default(),
        items: Vec::new(),
        yt_dlp_path: None,
        ffmpeg_path: None,
        deno_path: None,
    }
}

fn persist(runtime: &Runtime) -> Result<(), String> {
    let state = StoredState {
        items: runtime.state.items.clone(),
    };
    let state_json = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
    atomic_write(&runtime.paths.join(STATE_FILE), state_json.as_bytes())?;
    let settings_json =
        serde_json::to_string_pretty(&runtime.state.settings).map_err(|e| e.to_string())?;
    atomic_write(&runtime.paths.join(SETTINGS_FILE), settings_json.as_bytes())
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temp, contents).map_err(|e| e.to_string())?;
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.to_string());
    }
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn mark_updated(item: &mut DownloadItem) {
    item.updated_at = now_unix();
}

fn legacy_policy(mode: LegacyMp4Compatibility) -> (QualityMode, OutputCodec, bool) {
    match mode {
        LegacyMp4Compatibility::H264Preferred => {
            (QualityMode::CompatibilityFirst, OutputCodec::H264, false)
        }
        LegacyMp4Compatibility::H264Only => {
            (QualityMode::CompatibilityFirst, OutputCodec::H264, true)
        }
        LegacyMp4Compatibility::H265Vp9Preferred => {
            (QualityMode::QualityFirst, OutputCodec::Vp9, false)
        }
        LegacyMp4Compatibility::H265Vp9Only => (QualityMode::QualityFirst, OutputCodec::Vp9, true),
    }
}

fn normalize_settings(settings: &mut Settings) {
    if let Some(legacy) = settings.legacy_mp4_compatibility.take() {
        let (quality_mode, output_codec, transcode_if_needed) = legacy_policy(legacy);
        settings.quality_mode = quality_mode;
        settings.output_codec = output_codec;
        settings.transcode_if_needed = transcode_if_needed;
    }
}

fn normalize_item(item: &mut DownloadItem) {
    if let Some(legacy) = item.legacy_mp4_compatibility.take() {
        let (quality_mode, output_codec, transcode_if_needed) = legacy_policy(legacy);
        item.quality_mode = quality_mode;
        item.output_codec = output_codec;
        item.transcode_if_needed = transcode_if_needed;
    }
}

fn find_executable(name: &str) -> Option<String> {
    let mut candidates = Vec::new();
    if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from(format!("/opt/homebrew/bin/{name}")));
        candidates.push(PathBuf::from(format!("/usr/local/bin/{name}")));
        candidates.push(PathBuf::from(format!("/usr/bin/{name}")));
        if name == "deno" {
            if let Some(home) = dirs::home_dir() {
                candidates.push(home.join(".deno/bin/deno"));
            }
        }
    }
    if cfg!(target_os = "windows") {
        candidates.push(PathBuf::from(format!(
            r"C:\\Program Files\\yt-dlp\\{name}.exe"
        )));
        candidates.push(PathBuf::from(format!(
            r"C:\\Users\\Public\\scoop\\shims\\{name}.exe"
        )));
        candidates.push(PathBuf::from(format!("{name}.exe")));
        if name == "deno" {
            if let Some(home) = dirs::home_dir() {
                candidates.push(home.join(".deno/bin/deno.exe"));
            }
        }
    }
    candidates.push(PathBuf::from(name));
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            candidates.push(dir.join(name));
            if cfg!(target_os = "windows") {
                candidates.push(dir.join(format!("{name}.exe")));
            }
        }
    }
    candidates
        .into_iter()
        .find(|path| path.is_absolute() && path.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

fn emit_state(app: &AppHandle, runtime: &Runtime) {
    let _ = app.emit("queue-updated", &runtime.state);
}

fn expand_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

fn active_count(runtime: &Runtime) -> usize {
    runtime
        .state
        .items
        .iter()
        .filter(|i| i.status == DownloadStatus::Downloading)
        .count()
}

fn make_args(
    item: &DownloadItem,
    archive: &Path,
    ffmpeg_path: Option<&str>,
    deno_path: Option<&str>,
) -> Vec<String> {
    let mut args = item.profile_args.clone();
    if let Some(deno_path) = deno_path {
        if !item.profile_args.iter().any(|arg| arg == "--js-runtimes") {
            args.extend(["--js-runtimes".into(), format!("deno:{deno_path}")]);
        }
    }
    args.extend([
        "--newline".into(),
        "--continue".into(),
        "--no-playlist".into(),
        "--download-archive".into(),
        archive.to_string_lossy().into_owned(),
        "--progress-template".into(),
        "download:%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress._percent_str)s|%(progress._speed_str)s|%(progress._eta_str)s".into(),
        "--print".into(),
        "before_dl:__YTDLP_GUI_ITEM__%(title)s".into(),
        "--print".into(),
        "after_move:__YTDLP_GUI_PATH__%(filepath)s".into(),
        "-P".into(),
        expand_path(&item.output_dir).to_string_lossy().into_owned(),
        item.url.clone(),
    ]);
    if profile_targets_mp4(&item.profile_args)
        && !item
            .profile_args
            .iter()
            .any(|arg| arg == "-S" || arg == "--format-sort")
    {
        let sort_order = format_sort_order(&item.quality_mode, &item.output_codec);
        args.splice(
            args.len() - 1..args.len() - 1,
            ["--format-sort".into(), sort_order],
        );
    }
    if item.force_redownload {
        args.splice(
            args.len() - 1..args.len() - 1,
            [
                "--no-download-archive".into(),
                "--force-overwrites".into(),
                "--no-continue".into(),
            ],
        );
    }
    if let Some(ffmpeg_path) = ffmpeg_path {
        args.splice(
            args.len() - 1..args.len() - 1,
            ["--ffmpeg-location".into(), ffmpeg_path.into()],
        );
    }
    args
}

fn format_sort_order(quality_mode: &QualityMode, output_codec: &OutputCodec) -> String {
    let codec_order = match output_codec {
        OutputCodec::Auto => "res,fps,hdr,vcodec,acodec,br",
        OutputCodec::Vp9 => "res,fps,hdr,vcodec:vp9,acodec:opus,br",
        OutputCodec::H264 => "res,fps,hdr,vcodec:avc,acodec:aac,br",
    };
    match quality_mode {
        QualityMode::QualityFirst => codec_order.to_string(),
        QualityMode::CompatibilityFirst => match output_codec {
            OutputCodec::Auto => "vcodec:avc,acodec:aac,res,fps,hdr,br".into(),
            OutputCodec::Vp9 => "vcodec:vp9,acodec:opus,res,fps,hdr,br".into(),
            OutputCodec::H264 => "vcodec:avc,acodec:aac,res,fps,hdr,br".into(),
        },
    }
}

fn profile_targets_mp4(profile_args: &[String]) -> bool {
    profile_args.windows(2).any(|pair| {
        matches!(
            pair[0].as_str(),
            "--merge-output-format" | "--remux-video" | "--recode-video"
        ) && pair[1]
            .split('/')
            .any(|format| format.eq_ignore_ascii_case("mp4"))
    })
}

fn clean_progress_line(input: &str) -> String {
    let mut cleaned = String::with_capacity(input.len());
    let mut escape = false;
    for character in input.chars() {
        if escape {
            if character.is_ascii_alphabetic() {
                escape = false;
            }
            continue;
        }
        if character == '\u{1b}' {
            escape = true;
        } else if character != '\r' {
            cleaned.push(character);
        }
    }
    cleaned
}

fn parse_progress_number(value: &str) -> Option<f32> {
    value.trim().trim_end_matches('%').parse::<f32>().ok()
}

#[derive(Debug, PartialEq)]
struct DownloadProgressSample {
    percent: Option<f32>,
    speed: String,
    eta: String,
}

fn parse_download_progress(line: &str) -> Option<DownloadProgressSample> {
    let progress = line.strip_prefix("download:")?;
    let parts: Vec<&str> = progress.split('|').collect();
    let (percent, speed, eta) = if parts.len() >= 6 {
        let percent = parse_progress_number(parts[3]).or_else(|| {
            let downloaded = parse_progress_number(parts[0])?;
            let total =
                parse_progress_number(parts[1]).or_else(|| parse_progress_number(parts[2]))?;
            (total > 0.0).then_some(downloaded / total * 100.0)
        });
        (percent, parts[4], parts[5])
    } else {
        (
            parts.first().and_then(|part| parse_progress_number(part)),
            parts.get(1).copied().unwrap_or_default(),
            parts.get(2).copied().unwrap_or_default(),
        )
    };
    Some(DownloadProgressSample {
        percent,
        speed: speed.trim().to_string(),
        eta: eta.trim().to_string(),
    })
}

fn probe_duration_seconds(ffprobe_path: Option<&str>, input: &Path) -> Option<f64> {
    let ffprobe_path = ffprobe_path?;
    let output = Command::new(ffprobe_path)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(input)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|duration| *duration > 0.0)
}

fn probe_mp4_codecs(
    ffprobe_path: Option<&str>,
    input: &Path,
) -> Option<(Vec<String>, Vec<String>)> {
    let Some(ffprobe_path) = ffprobe_path else {
        return None;
    };
    let Ok(output) = Command::new(ffprobe_path)
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(input)
        .output()
    else {
        return None;
    };
    if !output.status.success() {
        return None;
    }

    let mut video_codecs = Vec::new();
    let mut audio_codecs = Vec::new();
    let probe_output = String::from_utf8_lossy(&output.stdout);
    for line in probe_output.lines() {
        let mut parts = line.split(',');
        let first = parts.next().unwrap_or_default();
        let second = parts.next().unwrap_or_default();
        let (stream_type, codec) = if matches!(first, "video" | "audio") {
            (first, second)
        } else {
            (second, first)
        };
        match stream_type {
            "video" => video_codecs.push(codec.to_string()),
            "audio" => audio_codecs.push(codec.to_string()),
            _ => {}
        }
    }

    Some((video_codecs, audio_codecs))
}

fn mp4_is_quicktime_compatible(ffprobe_path: Option<&str>, input: &Path) -> bool {
    let Some((video_codecs, audio_codecs)) = probe_mp4_codecs(ffprobe_path, input) else {
        return false;
    };
    video_codecs.len() == 1
        && video_codecs.first() == Some(&"h264".to_string())
        && audio_codecs
            .first()
            .map(|codec| codec == "aac")
            .unwrap_or(true)
}

fn maybe_make_mp4_compatible(
    _quality_mode: &QualityMode,
    output_codec: &OutputCodec,
    transcode_if_needed: bool,
    ffmpeg_path: Option<&str>,
    ffprobe_path: Option<&str>,
    output_path: Option<&str>,
    on_progress: &mut dyn FnMut(f32),
    on_started: &mut dyn FnMut(u32),
    on_finished: &mut dyn FnMut(),
    should_cancel: &dyn Fn() -> bool,
) -> Result<bool, String> {
    let Some(output_path) = output_path else {
        return Ok(false);
    };
    let input = Path::new(output_path);
    if input
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.eq_ignore_ascii_case("mp4"))
        != Some(true)
    {
        return Ok(false);
    }
    if !transcode_if_needed {
        return Ok(false);
    }
    let codecs = probe_mp4_codecs(ffprobe_path, input);
    let video_codec = codecs
        .as_ref()
        .and_then(|(video, _)| video.first())
        .map(String::as_str);
    let needs_transcode = match output_codec {
        OutputCodec::Auto => !mp4_is_quicktime_compatible(ffprobe_path, input),
        OutputCodec::H264 => !mp4_is_quicktime_compatible(ffprobe_path, input),
        OutputCodec::Vp9 => !matches!(video_codec, Some("vp9" | "vp9.2")),
    };
    if !needs_transcode {
        return Ok(false);
    }
    let Some(ffmpeg_path) = ffmpeg_path else {
        return Err("動画変換にはffmpegが必要です".into());
    };
    let target = match output_codec {
        OutputCodec::Auto | OutputCodec::H264 => TranscodeTarget::H264,
        OutputCodec::Vp9 => TranscodeTarget::Vp9,
    };
    let duration_seconds = probe_duration_seconds(ffprobe_path, input);
    on_progress(0.0);
    transcode_mp4(
        ffmpeg_path,
        input,
        target,
        duration_seconds,
        on_progress,
        on_started,
        on_finished,
        should_cancel,
    )?;
    Ok(true)
}

fn terminate_pid_tree(pid: u32) {
    let pid = pid.to_string();
    if cfg!(target_os = "windows") {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid, "/T", "/F"])
            .status();
    } else {
        let _ = Command::new("pkill").args(["-TERM", "-P", &pid]).status();
        let _ = Command::new("kill").args(["-TERM", &pid]).status();
    }
}

fn terminate_process_tree(child: &mut Child) {
    terminate_pid_tree(child.id());
    let _ = child.kill();
}

fn stop_all_processes(runtime: &mut Runtime) {
    for child in runtime.processes.values() {
        if let Ok(mut child) = child.lock() {
            terminate_process_tree(&mut child);
        }
    }
    for pid in runtime
        .ffmpeg_processes
        .values()
        .copied()
        .collect::<Vec<_>>()
    {
        terminate_pid_tree(pid);
    }
    runtime.ffmpeg_processes.clear();
    runtime.cancelled.extend(
        runtime
            .state
            .items
            .iter()
            .filter(|item| item.status == DownloadStatus::Downloading)
            .map(|item| item.id.clone()),
    );
    for item in &mut runtime.state.items {
        if item.status == DownloadStatus::Downloading {
            item.phase.clear();
            item.phase_progress = 0.0;
            item.status = DownloadStatus::Interrupted;
            item.error = Some("アプリ終了時に中断されました".into());
            item.error_log.push("アプリ終了時に中断されました".into());
            if item.error_log.len() > 200 {
                item.error_log.remove(0);
            }
            mark_updated(item);
        }
    }
    let _ = persist(runtime);
}

fn launch_queued(app: &AppHandle, shared: &SharedRuntime) {
    loop {
        let (id, executable, args, ffmpeg_path, ffprobe_path) = {
            let mut runtime = match shared.lock() {
                Ok(r) => r,
                Err(_) => return,
            };
            if active_count(&runtime) >= runtime.state.settings.max_concurrent_downloads {
                return;
            }
            let executable = runtime.state.yt_dlp_path.clone();
            let archive = runtime.paths.join(ARCHIVE_FILE);
            let ffmpeg_path = runtime.state.ffmpeg_path.clone();
            let ffprobe_path = find_executable("ffprobe");
            let deno_path = runtime.state.deno_path.clone();
            let Some(item) = runtime
                .state
                .items
                .iter_mut()
                .find(|i| i.status == DownloadStatus::Queued)
            else {
                return;
            };
            let id = item.id.clone();
            item.status = DownloadStatus::Downloading;
            item.phase = "downloading".into();
            item.phase_progress = 0.0;
            item.progress = 0.0;
            item.error = None;
            mark_updated(item);
            let args = make_args(item, &archive, ffmpeg_path.as_deref(), deno_path.as_deref());
            runtime.cancelled.remove(&id);
            let _ = persist(&runtime);
            emit_state(app, &runtime);
            (id, executable, args, ffmpeg_path, ffprobe_path)
        };

        let Some(executable) = executable else {
            let mut runtime = match shared.lock() {
                Ok(r) => r,
                Err(_) => return,
            };
            if let Some(item) = runtime.state.items.iter_mut().find(|i| i.id == id) {
                item.status = DownloadStatus::Failed;
                item.error = Some("yt-dlp not found".into());
                mark_updated(item);
            }
            let _ = persist(&runtime);
            emit_state(app, &runtime);
            continue;
        };

        let child = Command::new(&executable)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let Ok(mut child) = child else {
            let mut runtime = match shared.lock() {
                Ok(r) => r,
                Err(_) => return,
            };
            if let Some(item) = runtime.state.items.iter_mut().find(|i| i.id == id) {
                item.status = DownloadStatus::Failed;
                let message = format!("yt-dlpを起動できません: {executable}");
                item.error = Some(message.clone());
                item.error_log.push(message);
                mark_updated(item);
            }
            let _ = persist(&runtime);
            emit_state(app, &runtime);
            continue;
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let child = Arc::new(Mutex::new(child));
        if let Ok(mut runtime) = shared.lock() {
            runtime.processes.insert(id.clone(), child.clone());
        }
        let app = app.clone();
        let shared = shared.clone();
        thread::spawn(move || {
            let (tx, rx) = mpsc::channel::<String>();
            if let Some(stdout) = stdout {
                let tx = tx.clone();
                thread::spawn(move || {
                    for line in BufReader::new(stdout).lines().flatten() {
                        let _ = tx.send(line);
                    }
                });
            }
            if let Some(stderr) = stderr {
                let tx = tx.clone();
                thread::spawn(move || {
                    for line in BufReader::new(stderr).lines().flatten() {
                        let _ = tx.send(format!("__ERR__{line}"));
                    }
                });
            }
            drop(tx);
            let mut saw_item_marker = false;
            let mut output_path: Option<String> = None;
            let mut finished = None;
            let mut channel_closed = false;
            loop {
                if finished.is_none() {
                    finished = child
                        .lock()
                        .ok()
                        .and_then(|mut c| c.try_wait().ok().flatten());
                }

                match rx.recv_timeout(Duration::from_millis(80)) {
                    Ok(line) => {
                        if let Ok(mut runtime) = shared.lock() {
                            if let Some(item) = runtime.state.items.iter_mut().find(|i| i.id == id)
                            {
                                let line = clean_progress_line(&line);
                                let progress_line =
                                    line.strip_prefix("__ERR__").unwrap_or(&line).trim_start();
                                if let Some(title) = line.strip_prefix("__YTDLP_GUI_ITEM__") {
                                    item.title = title.trim().into();
                                    saw_item_marker = true;
                                } else if let Some(path) = line.strip_prefix("__YTDLP_GUI_PATH__") {
                                    let path = path.trim();
                                    if !path.is_empty() {
                                        output_path = Some(path.into());
                                        item.output_path = output_path.clone();
                                    }
                                } else if let Some(progress) =
                                    parse_download_progress(progress_line)
                                {
                                    if let Some(percent) = progress.percent {
                                        item.phase_progress = percent.clamp(0.0, 100.0);
                                        item.progress = item.phase_progress;
                                        item.phase = "downloading".into();
                                    }
                                    item.speed = progress.speed;
                                    item.eta = progress.eta;
                                } else if let Some(error) = line.strip_prefix("__ERR__") {
                                    let error = error.trim();
                                    if !error.is_empty() {
                                        item.error_log.push(error.into());
                                        if item.error_log.len() > 200 {
                                            item.error_log.remove(0);
                                        }
                                        if is_fatal_error(error) {
                                            item.error = Some(error.into());
                                        }
                                    }
                                }
                                mark_updated(item);
                            }
                            if runtime.last_persist.elapsed() >= Duration::from_secs(1) {
                                let _ = persist(&runtime);
                                runtime.last_persist = Instant::now();
                            }
                            emit_state(&app, &runtime);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => channel_closed = true,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }

                if let Some(exit) = finished {
                    if !channel_closed {
                        continue;
                    }
                    if exit.success() {
                        if let Ok(mut runtime) = shared.lock() {
                            if let Some(item) = runtime.state.items.iter_mut().find(|i| i.id == id)
                            {
                                if item.status == DownloadStatus::Downloading {
                                    item.phase = "downloaded".into();
                                    item.phase_progress = 100.0;
                                    item.progress = 100.0;
                                    mark_updated(item);
                                }
                            }
                            let _ = persist(&runtime);
                            emit_state(&app, &runtime);
                        }
                        thread::sleep(Duration::from_millis(240));
                    }
                    let conversion_result = if exit.success() && saw_item_marker {
                        let item_config = shared.lock().ok().and_then(|mut runtime| {
                            runtime.processes.remove(&id);
                            runtime
                                .state
                                .items
                                .iter()
                                .find(|item| item.id == id)
                                .map(|item| {
                                    (
                                        item.quality_mode.clone(),
                                        item.output_codec.clone(),
                                        item.transcode_if_needed,
                                        output_path.clone().or_else(|| item.output_path.clone()),
                                    )
                                })
                        });
                        item_config
                            .map(|(quality_mode, output_codec, transcode_if_needed, path)| {
                                let mut report_conversion_progress = |percent: f32| {
                                    if let Ok(mut runtime) = shared.lock() {
                                        if let Some(item) = runtime
                                            .state
                                            .items
                                            .iter_mut()
                                            .find(|item| item.id == id)
                                        {
                                            if item.status == DownloadStatus::Downloading {
                                                item.phase = "converting".into();
                                                item.phase_progress = percent.clamp(0.0, 100.0);
                                                item.progress = item.phase_progress;
                                                mark_updated(item);
                                            }
                                        }
                                        if runtime.last_persist.elapsed() >= Duration::from_secs(1)
                                        {
                                            let _ = persist(&runtime);
                                            runtime.last_persist = Instant::now();
                                        }
                                        emit_state(&app, &runtime);
                                    }
                                };
                                let shared_for_started = shared.clone();
                                let id_for_started = id.clone();
                                let mut register_ffmpeg = move |pid: u32| {
                                    if let Ok(mut runtime) = shared_for_started.lock() {
                                        runtime
                                            .ffmpeg_processes
                                            .insert(id_for_started.clone(), pid);
                                        if runtime.cancelled.contains(&id_for_started) {
                                            terminate_pid_tree(pid);
                                        }
                                    }
                                };
                                let shared_for_finished = shared.clone();
                                let id_for_finished = id.clone();
                                let mut unregister_ffmpeg = move || {
                                    if let Ok(mut runtime) = shared_for_finished.lock() {
                                        runtime.ffmpeg_processes.remove(&id_for_finished);
                                    }
                                };
                                let shared_for_cancel = shared.clone();
                                let id_for_cancel = id.clone();
                                let should_cancel = move || {
                                    shared_for_cancel
                                        .lock()
                                        .map(|runtime| runtime.cancelled.contains(&id_for_cancel))
                                        .unwrap_or(true)
                                };
                                maybe_make_mp4_compatible(
                                    &quality_mode,
                                    &output_codec,
                                    transcode_if_needed,
                                    ffmpeg_path.as_deref(),
                                    ffprobe_path.as_deref(),
                                    path.as_deref(),
                                    &mut report_conversion_progress,
                                    &mut register_ffmpeg,
                                    &mut unregister_ffmpeg,
                                    &should_cancel,
                                )
                            })
                            .unwrap_or(Ok(false))
                    } else {
                        if let Ok(mut runtime) = shared.lock() {
                            runtime.processes.remove(&id);
                        }
                        Ok(false)
                    };

                    if exit.success() && matches!(&conversion_result, Ok(true)) {
                        if let Ok(mut runtime) = shared.lock() {
                            if let Some(item) = runtime.state.items.iter_mut().find(|i| i.id == id)
                            {
                                if item.status == DownloadStatus::Downloading {
                                    item.phase = "encoded".into();
                                    item.phase_progress = 100.0;
                                    item.progress = 100.0;
                                    mark_updated(item);
                                }
                            }
                            let _ = persist(&runtime);
                            emit_state(&app, &runtime);
                        }
                        thread::sleep(Duration::from_millis(240));
                    }

                    if let Ok(mut runtime) = shared.lock() {
                        runtime.processes.remove(&id);
                        runtime.ffmpeg_processes.remove(&id);
                        runtime.cancelled.remove(&id);
                        let mut duplicate = false;
                        if let Some(item) = runtime.state.items.iter_mut().find(|i| i.id == id) {
                            if item.status == DownloadStatus::Downloading {
                                if exit.success() && saw_item_marker {
                                    if let Err(error) = conversion_result {
                                        item.status = DownloadStatus::Failed;
                                        item.error = Some(error.clone());
                                        item.error_log.push(error);
                                    } else {
                                        item.status = DownloadStatus::Completed;
                                        item.progress = 100.0;
                                        item.eta.clear();
                                    }
                                } else if exit.success() {
                                    item.status = DownloadStatus::Completed;
                                    item.progress = 100.0;
                                    if item.title == item.url {
                                        item.title = "すでにダウンロード済みです".into();
                                        duplicate = true;
                                    }
                                } else {
                                    item.status = DownloadStatus::Failed;
                                    if item.error.is_none() {
                                        let message =
                                            format!("終了コード: {}", exit.code().unwrap_or(-1));
                                        item.error = Some(message.clone());
                                        item.error_log.push(message);
                                    }
                                }
                                item.phase.clear();
                                item.force_redownload = false;
                                mark_updated(item);
                            }
                        }
                        let _ = persist(&runtime);
                        emit_state(&app, &runtime);
                        if duplicate {
                            let _ = app.emit("download-notice", "すでにダウンロード済みです");
                        }
                    }
                    launch_queued(&app, &shared);
                    return;
                }
                thread::sleep(Duration::from_millis(120));
            }
        });
    }
}

#[tauri::command]
fn get_app_state(state: State<'_, SharedRuntime>) -> Result<AppState, String> {
    state
        .lock()
        .map(|r| r.state.clone())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn refresh_tools(app: AppHandle, state: State<'_, SharedRuntime>) -> Result<AppState, String> {
    let mut runtime = state.lock().map_err(|e| e.to_string())?;
    runtime.state.yt_dlp_path = find_executable("yt-dlp");
    runtime.state.ffmpeg_path = find_executable("ffmpeg");
    runtime.state.deno_path = find_executable("deno");
    let snapshot = runtime.state.clone();
    emit_state(&app, &runtime);
    Ok(snapshot)
}

#[tauri::command]
fn add_downloads(
    app: AppHandle,
    state: State<'_, SharedRuntime>,
    urls: Vec<String>,
) -> Result<(), String> {
    let mut runtime = state.lock().map_err(|e| e.to_string())?;
    let settings = runtime.state.settings.clone();
    let now = now_unix();
    let mut skipped = 0usize;
    for url in urls
        .into_iter()
        .map(|u| u.trim().to_string())
        .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
    {
        let already_queued = runtime.state.items.iter().any(|item| {
            item.url == url
                && matches!(
                    item.status,
                    DownloadStatus::Queued
                        | DownloadStatus::Downloading
                        | DownloadStatus::Paused
                        | DownloadStatus::Interrupted
                )
        });
        if already_queued {
            skipped += 1;
            continue;
        }
        runtime.state.items.push(DownloadItem {
            id: uuid_like(),
            url: url.clone(),
            title: url,
            progress: 0.0,
            speed: String::new(),
            eta: String::new(),
            status: DownloadStatus::Queued,
            phase: String::new(),
            phase_progress: 0.0,
            error: None,
            error_log: Vec::new(),
            output_dir: settings.default_folder.clone(),
            profile: settings.default_profile.clone(),
            profile_args: settings.profile_args.clone(),
            quality_mode: settings.quality_mode.clone(),
            output_codec: settings.output_codec.clone(),
            transcode_if_needed: settings.transcode_if_needed,
            legacy_mp4_compatibility: None,
            force_redownload: false,
            output_path: None,
            created_at: now,
            updated_at: now,
        });
    }
    persist(&runtime)?;
    emit_state(&app, &runtime);
    if skipped > 0 {
        let _ = app.emit(
            "download-notice",
            format!("{}件はすでにキューにあります", skipped),
        );
    }
    drop(runtime);
    launch_queued(&app, &state.inner().clone());
    Ok(())
}

#[tauri::command]
fn resume_interrupted(app: AppHandle, state: State<'_, SharedRuntime>) -> Result<(), String> {
    let mut runtime = state.lock().map_err(|e| e.to_string())?;
    if runtime.state.settings.resume_on_launch {
        for item in &mut runtime.state.items {
            if item.status == DownloadStatus::Interrupted {
                item.status = DownloadStatus::Queued;
            }
        }
    }
    persist(&runtime)?;
    emit_state(&app, &runtime);
    drop(runtime);
    launch_queued(&app, &state.inner().clone());
    Ok(())
}

#[tauri::command]
fn pause_download(
    app: AppHandle,
    state: State<'_, SharedRuntime>,
    id: String,
) -> Result<(), String> {
    let mut runtime = state.lock().map_err(|e| e.to_string())?;
    runtime.cancelled.insert(id.clone());
    if let Some(child) = runtime.processes.get(&id) {
        let mut child = child.lock().map_err(|e| e.to_string())?;
        terminate_process_tree(&mut child);
    }
    if let Some(pid) = runtime.ffmpeg_processes.get(&id).copied() {
        terminate_pid_tree(pid);
    }
    if let Some(item) = runtime.state.items.iter_mut().find(|i| i.id == id) {
        item.status = DownloadStatus::Paused;
        item.phase.clear();
        item.phase_progress = 0.0;
        mark_updated(item);
    }
    persist(&runtime)?;
    emit_state(&app, &runtime);
    Ok(())
}

#[tauri::command]
fn retry_download(
    app: AppHandle,
    state: State<'_, SharedRuntime>,
    id: String,
) -> Result<(), String> {
    let mut runtime = state.lock().map_err(|e| e.to_string())?;
    runtime.cancelled.remove(&id);
    if let Some(item) = runtime.state.items.iter_mut().find(|i| i.id == id) {
        item.status = DownloadStatus::Queued;
        item.phase.clear();
        item.phase_progress = 0.0;
        item.force_redownload = false;
        item.progress = 0.0;
        item.error = None;
        item.error_log.clear();
        item.output_path = None;
        mark_updated(item);
    }
    persist(&runtime)?;
    emit_state(&app, &runtime);
    drop(runtime);
    launch_queued(&app, &state.inner().clone());
    Ok(())
}

#[tauri::command]
fn force_retry_download(
    app: AppHandle,
    state: State<'_, SharedRuntime>,
    id: String,
) -> Result<(), String> {
    let mut runtime = state.lock().map_err(|e| e.to_string())?;
    runtime.cancelled.remove(&id);
    let current_profile_args = runtime.state.settings.profile_args.clone();
    let current_profile = runtime.state.settings.default_profile.clone();
    let current_quality_mode = runtime.state.settings.quality_mode.clone();
    let current_output_codec = runtime.state.settings.output_codec.clone();
    let current_transcode_if_needed = runtime.state.settings.transcode_if_needed;
    if let Some(item) = runtime.state.items.iter_mut().find(|i| i.id == id) {
        item.status = DownloadStatus::Queued;
        item.phase.clear();
        item.phase_progress = 0.0;
        item.profile_args = current_profile_args;
        item.profile = current_profile;
        item.quality_mode = current_quality_mode;
        item.output_codec = current_output_codec;
        item.transcode_if_needed = current_transcode_if_needed;
        item.force_redownload = true;
        item.progress = 0.0;
        item.speed.clear();
        item.eta.clear();
        item.error = None;
        item.error_log.clear();
        item.output_path = None;
        mark_updated(item);
    }
    persist(&runtime)?;
    emit_state(&app, &runtime);
    drop(runtime);
    launch_queued(&app, &state.inner().clone());
    Ok(())
}

#[tauri::command]
fn resume_download(
    app: AppHandle,
    state: State<'_, SharedRuntime>,
    id: String,
) -> Result<(), String> {
    let mut runtime = state.lock().map_err(|e| e.to_string())?;
    runtime.cancelled.remove(&id);
    if let Some(item) = runtime.state.items.iter_mut().find(|i| i.id == id) {
        if item.status == DownloadStatus::Paused {
            item.status = DownloadStatus::Queued;
            item.error = None;
            mark_updated(item);
        }
    }
    persist(&runtime)?;
    emit_state(&app, &runtime);
    drop(runtime);
    launch_queued(&app, &state.inner().clone());
    Ok(())
}

#[tauri::command]
fn remove_download(
    app: AppHandle,
    state: State<'_, SharedRuntime>,
    id: String,
) -> Result<(), String> {
    let mut runtime = state.lock().map_err(|e| e.to_string())?;
    runtime.cancelled.insert(id.clone());
    if let Some(child) = runtime.processes.get(&id) {
        let mut child = child.lock().map_err(|e| e.to_string())?;
        terminate_process_tree(&mut child);
    }
    if let Some(pid) = runtime.ffmpeg_processes.get(&id).copied() {
        terminate_pid_tree(pid);
    }
    runtime.state.items.retain(|i| i.id != id);
    runtime.processes.remove(&id);
    runtime.ffmpeg_processes.remove(&id);
    persist(&runtime)?;
    emit_state(&app, &runtime);
    drop(runtime);
    launch_queued(&app, &state.inner().clone());
    Ok(())
}

#[tauri::command]
fn clear_queue(app: AppHandle, state: State<'_, SharedRuntime>) -> Result<(), String> {
    let mut runtime = state.lock().map_err(|e| e.to_string())?;
    runtime.state.items.retain(|item| {
        !matches!(
            item.status,
            DownloadStatus::Queued | DownloadStatus::Interrupted
        )
    });
    persist(&runtime)?;
    emit_state(&app, &runtime);
    Ok(())
}

#[tauri::command]
fn clear_history(app: AppHandle, state: State<'_, SharedRuntime>) -> Result<(), String> {
    let mut runtime = state.lock().map_err(|e| e.to_string())?;
    runtime.state.items.retain(|item| {
        !matches!(
            item.status,
            DownloadStatus::Completed | DownloadStatus::Failed
        )
    });
    persist(&runtime)?;
    emit_state(&app, &runtime);
    Ok(())
}

#[tauri::command]
fn reorder_downloads(
    app: AppHandle,
    state: State<'_, SharedRuntime>,
    ids: Vec<String>,
) -> Result<(), String> {
    let mut runtime = state.lock().map_err(|e| e.to_string())?;
    let mut ordered = Vec::new();
    for id in ids {
        if let Some(pos) = runtime.state.items.iter().position(|i| i.id == id) {
            ordered.push(runtime.state.items.remove(pos));
        }
    }
    ordered.append(&mut runtime.state.items);
    runtime.state.items = ordered;
    persist(&runtime)?;
    emit_state(&app, &runtime);
    Ok(())
}

#[tauri::command]
fn save_settings(
    app: AppHandle,
    state: State<'_, SharedRuntime>,
    settings: Settings,
) -> Result<(), String> {
    let mut runtime = state.lock().map_err(|e| e.to_string())?;
    runtime.state.settings = Settings {
        max_concurrent_downloads: settings.max_concurrent_downloads.clamp(1, 8),
        default_folder: settings.default_folder.trim().into(),
        default_profile: settings.default_profile.trim().into(),
        resume_on_launch: settings.resume_on_launch,
        profile_args: settings
            .profile_args
            .into_iter()
            .filter(|a| !a.trim().is_empty())
            .collect(),
        theme: settings.theme,
        quality_mode: settings.quality_mode,
        output_codec: settings.output_codec,
        transcode_if_needed: settings.transcode_if_needed,
        legacy_mp4_compatibility: None,
    };
    persist(&runtime)?;
    emit_state(&app, &runtime);
    drop(runtime);
    launch_queued(&app, &state.inner().clone());
    Ok(())
}

fn uuid_like() -> String {
    use std::time::SystemTime;
    format!(
        "{}-{}",
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id()
    )
}

fn is_fatal_error(line: &str) -> bool {
    let upper = line.trim_start().to_ascii_uppercase();
    !upper.starts_with("WARNING:") && (upper.contains("ERROR") || upper.contains("FATAL"))
}

#[cfg(test)]
mod tests {
    use super::{
        find_executable, format_sort_order, is_fatal_error, parse_download_progress,
        probe_duration_seconds, probe_mp4_codecs, transcode_mp4, uuid_like, OutputCodec,
        QualityMode, TranscodeTarget,
    };
    use std::process::Command;

    #[test]
    fn warning_is_not_fatal() {
        assert!(!is_fatal_error(
            "WARNING: No supported JavaScript runtime could be found"
        ));
    }

    #[test]
    fn error_is_fatal() {
        assert!(is_fatal_error("ERROR: Postprocessing failed"));
    }

    #[test]
    fn fatal_is_fatal() {
        assert!(is_fatal_error("FATAL: invalid configuration"));
    }

    #[test]
    fn yt_dlp_progress_uses_percent_and_byte_fallback() {
        let direct =
            parse_download_progress("download:5242880|10485760|NA| 50.0%|1.00MiB/s|00:05").unwrap();
        assert_eq!(direct.percent, Some(50.0));
        assert_eq!(direct.speed, "1.00MiB/s");
        assert_eq!(direct.eta, "00:05");

        let fallback =
            parse_download_progress("download:5242880|NA|10485760|NA|1.00MiB/s|00:05").unwrap();
        assert_eq!(fallback.percent, Some(50.0));
    }

    #[test]
    fn format_sort_order_keeps_quality_and_codec_as_separate_axes() {
        assert_eq!(
            format_sort_order(&QualityMode::QualityFirst, &OutputCodec::H264),
            "res,fps,hdr,vcodec:avc,acodec:aac,br"
        );
        assert_eq!(
            format_sort_order(&QualityMode::CompatibilityFirst, &OutputCodec::Vp9),
            "vcodec:vp9,acodec:opus,res,fps,hdr,br"
        );
    }

    #[test]
    #[ignore = "requires local ffmpeg and ffprobe"]
    fn real_ffmpeg_conversion_reports_progress_and_reaches_h264() {
        let Some(ffmpeg) = find_executable("ffmpeg") else {
            return;
        };
        let Some(ffprobe) = find_executable("ffprobe") else {
            return;
        };
        let test_dir = std::env::temp_dir().join(format!("queuedesk-progress-{}", uuid_like()));
        std::fs::create_dir_all(&test_dir).unwrap();
        let input = test_dir.join("source.mp4");
        let generated = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=1280x720:rate=30:duration=8",
                "-c:v",
                "mpeg4",
                "-q:v",
                "5",
                "-an",
            ])
            .arg(&input)
            .status()
            .unwrap();
        assert!(generated.success());

        let duration = probe_duration_seconds(Some(&ffprobe), &input);
        let mut updates = Vec::new();
        transcode_mp4(
            &ffmpeg,
            &input,
            TranscodeTarget::H264,
            duration,
            &mut |percent| updates.push(percent),
            &mut |_| {},
            &mut || {},
            &|| false,
        )
        .unwrap();

        assert!(updates.len() >= 2, "ffmpeg progress updates: {updates:?}");
        assert!(updates.last().is_some_and(|percent| *percent == 100.0));
        assert!(updates.windows(2).all(|pair| pair[0] <= pair[1]));
        let codecs = probe_mp4_codecs(Some(&ffprobe), &input).unwrap();
        assert_eq!(codecs.0.first().map(String::as_str), Some("h264"));
        let _ = std::fs::remove_dir_all(test_dir);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let runtime = load_runtime(&app.handle()).map_err(std::io::Error::other)?;
            app.manage(Arc::new(Mutex::new(runtime)));
            Ok(())
        })
        .on_window_event(|window, event| {
            if matches!(event, WindowEvent::CloseRequested { .. }) {
                if let Some(shared) = window.app_handle().try_state::<SharedRuntime>() {
                    if let Ok(mut runtime) = shared.lock() {
                        stop_all_processes(&mut runtime);
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_app_state,
            refresh_tools,
            add_downloads,
            resume_interrupted,
            pause_download,
            retry_download,
            force_retry_download,
            resume_download,
            remove_download,
            clear_queue,
            clear_history,
            reorder_downloads,
            save_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
