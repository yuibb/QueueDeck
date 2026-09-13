use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};

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
enum Mp4Compatibility {
    NativePreferred,
    TranscodeMaximum,
}

impl Default for Mp4Compatibility {
    fn default() -> Self {
        Self::NativePreferred
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
    error: Option<String>,
    #[serde(default)]
    error_log: Vec<String>,
    output_dir: String,
    profile: String,
    #[serde(default)]
    profile_args: Vec<String>,
    #[serde(default)]
    mp4_compatibility: Mp4Compatibility,
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
    mp4_compatibility: Mp4Compatibility,
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
            mp4_compatibility: Mp4Compatibility::NativePreferred,
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

    let migrated_at = now_unix();
    for item in &mut state.items {
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
        "download:%(progress._percent_str)s|%(progress._speed_str)s|%(progress._eta_str)s".into(),
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
        let sort_order = match &item.mp4_compatibility {
            Mp4Compatibility::NativePreferred => "res,codec:avc:m4a",
            Mp4Compatibility::TranscodeMaximum => "res,br",
        };
        args.splice(
            args.len() - 1..args.len() - 1,
            ["--format-sort".into(), sort_order.into()],
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

fn mp4_is_quicktime_compatible(ffprobe_path: Option<&str>, input: &Path) -> bool {
    let Some(ffprobe_path) = ffprobe_path else {
        return false;
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
        return false;
    };
    if !output.status.success() {
        return false;
    }

    let mut video_codecs = Vec::new();
    let mut audio_codecs = Vec::new();
    let probe_output = String::from_utf8_lossy(&output.stdout);
    for line in probe_output.lines() {
        let mut parts = line.split(',');
        let stream_type = parts.next().unwrap_or_default();
        let codec = parts.next().unwrap_or_default();
        match stream_type {
            "video" => video_codecs.push(codec),
            "audio" => audio_codecs.push(codec),
            _ => {}
        }
    }

    video_codecs.len() == 1
        && video_codecs.first() == Some(&"h264")
        && audio_codecs
            .first()
            .map(|codec| *codec == "aac")
            .unwrap_or(true)
}

fn transcode_mp4_to_h264(ffmpeg_path: &str, input: &Path) -> Result<(), String> {
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
    let temporary = input.with_file_name(format!("{stem}.queuedesk-h264.tmp.mp4"));
    let _ = std::fs::remove_file(&temporary);
    let output = Command::new(ffmpeg_path)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(input)
        .args([
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-map_metadata",
            "0",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "20",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-movflags",
            "+faststart",
        ])
        .arg(&temporary)
        .output()
        .map_err(|error| format!("ffmpegを起動できません: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let _ = std::fs::remove_file(&temporary);
        return Err(if detail.is_empty() {
            format!(
                "H.264変換に失敗しました（終了コード: {:?}）",
                output.status.code()
            )
        } else {
            format!("H.264変換に失敗しました: {detail}")
        });
    }

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

fn maybe_make_mp4_compatible(
    mode: &Mp4Compatibility,
    ffmpeg_path: Option<&str>,
    ffprobe_path: Option<&str>,
    output_path: Option<&str>,
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
    let needs_transcode = match mode {
        Mp4Compatibility::TranscodeMaximum => true,
        Mp4Compatibility::NativePreferred => !mp4_is_quicktime_compatible(ffprobe_path, input),
    };
    if !needs_transcode {
        return Ok(false);
    }
    let Some(ffmpeg_path) = ffmpeg_path else {
        return Err("H.264変換にはffmpegが必要です".into());
    };
    transcode_mp4_to_h264(ffmpeg_path, input)?;
    Ok(true)
}

fn terminate_process_tree(child: &mut Child) {
    let pid = child.id().to_string();
    if cfg!(target_os = "windows") {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid, "/T", "/F"])
            .status();
    } else {
        let _ = Command::new("pkill").args(["-TERM", "-P", &pid]).status();
        let _ = child.kill();
    }
}

fn stop_all_processes(runtime: &mut Runtime) {
    for child in runtime.processes.values() {
        if let Ok(mut child) = child.lock() {
            terminate_process_tree(&mut child);
        }
    }
    for item in &mut runtime.state.items {
        if item.status == DownloadStatus::Downloading {
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
            item.error = None;
            mark_updated(item);
            let args = make_args(item, &archive, ffmpeg_path.as_deref(), deno_path.as_deref());
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
                                if let Some(title) = line.strip_prefix("__YTDLP_GUI_ITEM__") {
                                    item.title = title.trim().into();
                                    saw_item_marker = true;
                                } else if let Some(path) = line.strip_prefix("__YTDLP_GUI_PATH__") {
                                    let path = path.trim();
                                    if !path.is_empty() {
                                        output_path = Some(path.into());
                                        item.output_path = output_path.clone();
                                    }
                                } else if let Some(progress) = line.strip_prefix("download:") {
                                    let parts: Vec<&str> = progress.split('|').collect();
                                    if let Some(percent) = parts.first().and_then(|p| {
                                        p.trim().trim_end_matches('%').parse::<f32>().ok()
                                    }) {
                                        item.progress = percent;
                                    }
                                    if let Some(speed) = parts.get(1) {
                                        item.speed = speed.trim().into();
                                    }
                                    if let Some(eta) = parts.get(2) {
                                        item.eta = eta.trim().into();
                                    }
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
                                        item.mp4_compatibility.clone(),
                                        output_path.clone().or_else(|| item.output_path.clone()),
                                    )
                                })
                        });
                        item_config
                            .map(|(mode, path)| {
                                maybe_make_mp4_compatible(
                                    &mode,
                                    ffmpeg_path.as_deref(),
                                    ffprobe_path.as_deref(),
                                    path.as_deref(),
                                )
                            })
                            .unwrap_or(Ok(false))
                    } else {
                        if let Ok(mut runtime) = shared.lock() {
                            runtime.processes.remove(&id);
                        }
                        Ok(false)
                    };

                    if let Ok(mut runtime) = shared.lock() {
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
            error: None,
            error_log: Vec::new(),
            output_dir: settings.default_folder.clone(),
            profile: settings.default_profile.clone(),
            profile_args: settings.profile_args.clone(),
            mp4_compatibility: settings.mp4_compatibility.clone(),
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
    if let Some(child) = runtime.processes.get(&id) {
        let mut child = child.lock().map_err(|e| e.to_string())?;
        terminate_process_tree(&mut child);
    }
    if let Some(item) = runtime.state.items.iter_mut().find(|i| i.id == id) {
        item.status = DownloadStatus::Paused;
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
    if let Some(item) = runtime.state.items.iter_mut().find(|i| i.id == id) {
        item.status = DownloadStatus::Queued;
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
    let current_profile_args = runtime.state.settings.profile_args.clone();
    let current_profile = runtime.state.settings.default_profile.clone();
    let current_mp4_compatibility = runtime.state.settings.mp4_compatibility.clone();
    if let Some(item) = runtime.state.items.iter_mut().find(|i| i.id == id) {
        item.status = DownloadStatus::Queued;
        item.profile_args = current_profile_args;
        item.profile = current_profile;
        item.mp4_compatibility = current_mp4_compatibility;
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
    if let Some(child) = runtime.processes.get(&id) {
        let mut child = child.lock().map_err(|e| e.to_string())?;
        terminate_process_tree(&mut child);
    }
    runtime.state.items.retain(|i| i.id != id);
    runtime.processes.remove(&id);
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
        mp4_compatibility: settings.mp4_compatibility,
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
    use super::is_fatal_error;

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
