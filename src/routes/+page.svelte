<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { openUrl } from "@tauri-apps/plugin-opener";

  type Status = "queued" | "downloading" | "paused" | "completed" | "failed" | "interrupted";
  type Theme = "auto" | "light" | "dark";
  type Mp4Compatibility = "native_preferred" | "transcode_maximum";
  type DownloadItem = { id: string; url: string; title: string; progress: number; speed: string; eta: string; status: Status; error: string | null; error_log: string[]; output_dir: string; profile: string; profile_args: string[]; created_at: number; updated_at: number };
  type Settings = { max_concurrent_downloads: number; default_folder: string; default_profile: string; resume_on_launch: boolean; profile_args: string[]; theme: Theme; mp4_compatibility: Mp4Compatibility };
  type AppState = { settings: Settings; items: DownloadItem[]; yt_dlp_path: string | null; ffmpeg_path: string | null; deno_path: string | null };

  let appState: AppState = { settings: { max_concurrent_downloads: 3, default_folder: "~/Downloads/yt-dlp", default_profile: "Default", resume_on_launch: true, profile_args: [], theme: "auto", mp4_compatibility: "native_preferred" }, items: [], yt_dlp_path: null, ffmpeg_path: null, deno_path: null };
  let urlInput = "";
  let isDropActive = false;
  let showSettings = false;
  let settingsDraft: Settings = structuredClone(appState.settings);
  let toast = "";
  let draggedId: string | null = null;
  let isLoading = true;
  let generatorMode: "video" | "audio" = "video";
  let generatorQuality: "best" | "1080" | "720" = "best";
  let generatorContainer = "mp4";
  let generatorAudioFormat = "mp3";
  let generatorMetadata = true;
  let generatorThumbnail = true;
  let generatorSubtitles = false;
  let selectedErrorId: string | null = null;
  let systemThemeQuery: MediaQueryList | null = null;
  let toolSettingsElement: HTMLDivElement | null = null;

  $: activeItems = appState.items.filter((item) => item.status === "downloading" || item.status === "paused");
  $: queueItems = appState.items.filter((item) => item.status === "queued" || item.status === "interrupted");
  $: historyItems = appState.items.filter((item) => item.status === "completed" || item.status === "failed");
  $: hasToolIssue = appState.yt_dlp_path === null || appState.ffmpeg_path === null || appState.deno_path === null;
  $: showToolWarning = !isLoading && hasToolIssue;
  $: selectedError = selectedErrorId ? appState.items.find((item) => item.id === selectedErrorId) ?? null : null;

  function applyTheme(themeOverride?: Theme) {
    if (typeof document === "undefined") return;
    const theme = themeOverride ?? appState.settings.theme;
    const dark = theme === "dark" || (theme === "auto" && systemThemeQuery?.matches === true);
    document.body.classList.toggle("theme-dark", dark);
    document.body.classList.toggle("theme-light", !dark);
  }

  onMount(() => {
    let unlisten: (() => void) | undefined;
    let unlistenNotice: (() => void) | undefined;
    let disposed = false;
    systemThemeQuery = window.matchMedia("(prefers-color-scheme: dark)");
    const handleSystemThemeChange = () => applyTheme(showSettings ? settingsDraft.theme : undefined);
    systemThemeQuery.addEventListener?.("change", handleSystemThemeChange);
    applyTheme();
    async function initialize() {
      unlistenNotice = await listen<string>("download-notice", (event) => showToast(event.payload));
      unlisten = await listen<AppState>("queue-updated", (event) => { appState = event.payload; applyTheme(showSettings ? settingsDraft.theme : undefined); isLoading = false; });
      if (disposed) { unlisten(); unlistenNotice(); return; }
      try { appState = await invoke<AppState>("get_app_state"); settingsDraft = structuredClone(appState.settings); applyTheme(); await invoke("resume_interrupted"); }
      catch (error) { showToast(String(error)); }
      finally { isLoading = false; }
    }
    void initialize();
    return () => { disposed = true; unlisten?.(); unlistenNotice?.(); systemThemeQuery?.removeEventListener?.("change", handleSystemThemeChange); document.body.classList.remove("theme-dark", "theme-light"); };
  });

  function extractUrls(text: string): string[] {
    return [...text.matchAll(/https?:\/\/[^\s<>"']+/gi)].map((match) => match[0].replace(/[),.;!?]+$/, "")).filter((url, index, all) => all.indexOf(url) === index);
  }

  async function addUrls(text: string) {
    const urls = extractUrls(text);
    if (!urls.length) { showToast("URLを入力してください"); return; }
    try { await invoke("add_downloads", { urls }); urlInput = ""; } catch (error) { showToast(String(error)); }
  }

  function handlePaste(event: ClipboardEvent) {
    const text = event.clipboardData?.getData("text") ?? "";
    if (extractUrls(text).length) { event.preventDefault(); void addUrls(text); }
  }

  function handleDrop(event: DragEvent) {
    event.preventDefault(); isDropActive = false;
    void addUrls(event.dataTransfer?.getData("text/plain") ?? event.dataTransfer?.getData("text/uri-list") ?? "");
  }

  async function action(command: string, id: string) { try { await invoke(command, { id }); } catch (error) { showToast(String(error)); } }
  async function forceRetry(item: DownloadItem) { showToast("強制再取得をキューに追加しました"); await action("force_retry_download", item.id); }
  async function clearQueue() { try { await invoke("clear_queue"); showToast("キューをクリアしました"); } catch (error) { showToast(String(error)); } }
  async function clearHistory() { try { await invoke("clear_history"); showToast("履歴をクリアしました"); } catch (error) { showToast(String(error)); } }
  async function refreshTools() { try { appState = await invoke<AppState>("refresh_tools"); showToast("外部ツールの状態を再確認しました"); } catch (error) { showToast(String(error)); } }
  async function openExternal(url: string) { try { await openUrl(url); } catch (error) { showToast(String(error)); } }
  function openErrorDetails(item: DownloadItem) { selectedErrorId = item.id; }
  function errorLogText(item: DownloadItem) { return [`タイトル: ${item.title}`, `URL: ${item.url}`, `状態: ${statusLabel(item.status)}`, item.error ? `エラー: ${item.error}` : "", "", ...item.error_log].filter(Boolean).join("\n"); }
  async function copyErrorLog(item: DownloadItem) { try { await navigator.clipboard.writeText(errorLogText(item)); showToast("エラーログをコピーしました"); } catch (error) { showToast(`コピーできませんでした: ${String(error)}`); } }
  async function saveSettings() { try { const savedSettings = structuredClone(settingsDraft); await invoke("save_settings", { settings: savedSettings }); appState = { ...appState, settings: savedSettings }; showSettings = false; applyTheme(); showToast("設定を保存しました"); } catch (error) { showToast(String(error)); } }
  function openSettings() { settingsDraft = structuredClone(appState.settings); showSettings = true; applyTheme(settingsDraft.theme); if (hasToolIssue) window.setTimeout(() => toolSettingsElement?.scrollIntoView({ behavior: "smooth", block: "nearest" }), 0); }
  function closeSettings() { showSettings = false; applyTheme(); }

  function generateProfileArgs() {
    const args = generatorMode === "audio"
      ? ["-x", "--audio-format", generatorAudioFormat]
      : ["-f", generatorQuality === "best" ? "bv*+ba/b" : `bv*[height<=${generatorQuality}]+ba/b[height<=${generatorQuality}]`, "--merge-output-format", generatorContainer];
    if (generatorMetadata) args.push("--embed-metadata");
    if (generatorThumbnail) args.push("--embed-thumbnail");
    if (generatorSubtitles) args.push("--write-subs", "--sub-langs", "all");
    settingsDraft.profile_args = args;
    showToast("Profile argsを生成しました");
  }

  async function reorder(targetId: string) {
    if (!draggedId || draggedId === targetId) return;
    const ids = queueItems.map((item) => item.id); const from = ids.indexOf(draggedId); const to = ids.indexOf(targetId);
    ids.splice(from, 1); ids.splice(to, 0, draggedId); draggedId = null;
    try { await invoke("reorder_downloads", { ids }); } catch (error) { showToast(String(error)); }
  }

  function showToast(message: string) { toast = message; window.setTimeout(() => { if (toast === message) toast = ""; }, 4200); }
  function statusLabel(status: Status) { return ({ queued: "待機中", downloading: "ダウンロード中", paused: "一時停止", completed: "完了", failed: "失敗", interrupted: "中断" })[status]; }
  function shortTitle(item: DownloadItem) { return item.title === item.url ? item.url : item.title; }
</script>

<svelte:head><title>QueueDeck</title><meta name="description" content="A lightweight yt-dlp download manager" /></svelte:head>

<main class:drop-active={isDropActive} ondragover={(event) => { event.preventDefault(); isDropActive = true; }} ondragleave={() => isDropActive = false} ondrop={handleDrop}>
  <header class="topbar">
    <div class="brand"><span class="brand-mark"><img src="/icons/ytdlp-download-m3-02.png" alt="" /></span><div><h1>QueueDeck</h1><p>Light. Fast. Queued.</p></div></div>
    <div class="topbar-actions"><button class="profile-switcher" aria-label="Profile設定を開く" onclick={openSettings}><span>Profile</span><strong>{appState.settings.default_profile}</strong><span class="profile-chevron">⌄</span></button>{#if showToolWarning}<button class="tool-warning" aria-label="外部ツールに問題があります。設定を開く" title="外部ツールに問題があります" onclick={openSettings}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="m12 4 9 16H3L12 4Z" /><path d="M12 9v5m0 3h.01" /></svg></button>{/if}<button class="icon-button" aria-label="設定" onclick={openSettings}>⚙</button></div>
  </header>

  <section class="drop-zone" aria-label="URLを追加">
    <div class="drop-icon">↓</div><h2>URLをここにドロップ</h2><p>または、URLを貼り付けてください</p>
    <div class="input-row"><input bind:value={urlInput} onpaste={handlePaste} onkeydown={(event) => event.key === "Enter" && addUrls(urlInput)} placeholder="https://..." aria-label="ダウンロードURL" /><button class="primary" onclick={() => addUrls(urlInput)}>追加</button></div>
    <small>Default · {appState.settings.default_folder}</small>
  </section>

  {#if isLoading}<div class="empty-state">読み込み中…</div>{:else}
    <section class="section downloads-section"><div class="section-heading"><div><h2>ダウンロード</h2><span class="subheading">進行中・待機中</span></div><div class="queue-heading-actions"><span class="queue-count-control" class:has-items={queueItems.length > 0}><span class="count queue-count">{activeItems.length + queueItems.length}</span>{#if queueItems.length > 0}<button class="clear-queue-button" aria-label="待機中のキューをクリア" title="待機中のキューをクリア" onclick={clearQueue}><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 7h12m-9 0V5h6v2m-8 0 1 13h8l1-13M10 10v7m4-7v7" /></svg></button>{/if}</span></div></div>
      {#if activeItems.length === 0 && queueItems.length === 0}<div class="empty-state compact">ダウンロードはありません</div>{:else}<div class="download-list">
        {#each activeItems as item (item.id)}<article class="download-card" class:paused={item.status === "paused"} style={`--progress: ${Math.min(100, Math.max(0, item.progress))}%`}><div class="download-progress-fill" aria-hidden="true"></div><div class="card-top"><div class="download-symbol">↓</div><div class="item-info"><strong title={item.title}>{shortTitle(item)}</strong><span>{statusLabel(item.status)} · <b class="progress-percent">{item.progress.toFixed(0)}%</b></span></div>{#if item.status === "downloading"}<button class="text-button danger" onclick={() => action("pause_download", item.id)}>停止</button>{:else}<button class="text-button" onclick={() => action("resume_download", item.id)}>再開</button>{/if}</div><div class="metrics"><span>{item.speed || "—"}</span><span>ETA {item.eta || "—"}</span><span>{item.profile}</span></div></article>{/each}
        {#each queueItems as item (item.id)}<article class="queue-row" draggable="true" ondragstart={() => draggedId = item.id} ondragover={(event) => event.preventDefault()} ondrop={() => reorder(item.id)}><span class="drag-handle" aria-label="並び替え">⠿</span><div class="queue-title"><strong title={item.title}>{shortTitle(item)}</strong><span>{statusLabel(item.status)}{item.error && (item.status === "failed" || item.status === "interrupted") ? ` · ${item.error}` : ""}</span></div>{#if item.error_log.length > 0}<button class="text-button" onclick={() => openErrorDetails(item)}>ログ</button>{/if}{#if item.status === "failed" || item.status === "interrupted"}<button class="text-button" onclick={() => action("retry_download", item.id)}>再試行</button>{:else if item.status === "paused"}<button class="text-button" onclick={() => action("resume_download", item.id)}>再開</button>{/if}<button class="remove-button" aria-label="削除" onclick={() => action("remove_download", item.id)}>×</button></article>{/each}
      </div>{/if}
    </section>

    <section class="section history-section"><div class="section-heading"><div><h2>履歴</h2><span class="subheading">完了・失敗した項目</span></div><div class="queue-heading-actions"><span class="count">{historyItems.length}</span>{#if historyItems.length > 0}<button class="text-button danger" onclick={clearHistory}>履歴をクリア</button>{/if}</div></div>
      {#if historyItems.length === 0}<div class="empty-state compact">履歴はありません</div>{:else}<div class="queue-list">
        {#each historyItems as item (item.id)}<article class="queue-row history-row"><div class:history-success={item.status === "completed"} class:history-failed={item.status === "failed"} class="history-dot"></div><div class="queue-title"><strong title={item.title}>{shortTitle(item)}</strong><span>{statusLabel(item.status)}{item.error ? ` · ${item.error}` : ""}</span></div>{#if item.error_log.length > 0}<button class="text-button" onclick={() => openErrorDetails(item)}>ログ</button>{/if}{#if item.status === "completed"}<button class="text-button" onclick={() => forceRetry(item)}>強制再取得</button>{:else}<button class="text-button" onclick={() => action("retry_download", item.id)}>再試行</button>{/if}<button class="remove-button" aria-label="履歴から削除" onclick={() => action("remove_download", item.id)}>×</button></article>{/each}
      </div>{/if}
    </section>
  {/if}

</main>

{#if showSettings}
  <div class="modal-backdrop" role="presentation" onclick={(event) => event.target === event.currentTarget && closeSettings()}>
    <dialog open class="modal" aria-labelledby="settings-title">
      <div class="modal-heading"><div><h2 id="settings-title">設定</h2><p>ダウンロードの基本設定</p></div><button class="remove-button" aria-label="閉じる" onclick={closeSettings}>×</button></div>
      <label>同時ダウンロード数<input type="number" min="1" max="8" bind:value={settingsDraft.max_concurrent_downloads} /></label>
      <label>Default Folder<input bind:value={settingsDraft.default_folder} placeholder="~/Downloads/yt-dlp" /></label>
      <label>Default Profile<input bind:value={settingsDraft.default_profile} /></label>
      <label class="switch-row"><span>Resume on launch<small>前回中断した項目を起動時に再開</small></span><input class="switch" type="checkbox" bind:checked={settingsDraft.resume_on_launch} /></label>
      <div class="setting-field"><div class="setting-label-row"><span class="setting-label">MP4の再生互換性</span><button type="button" class="info-tip" aria-label="MP4の再生互換性の説明"><span class="info-glyph">i</span><span class="tooltip-bubble" role="tooltip">QuickTimeなどで再生しやすいH.264/AACにする方法を選びます。</span></button></div><select bind:value={settingsDraft.mp4_compatibility}><option value="native_preferred">H.264を優先（推奨）</option><option value="transcode_maximum">H.264へ変換（最大解像度）</option></select><small>{settingsDraft.mp4_compatibility === "native_preferred" ? "H.264があればそのまま取得し、なければ最大解像度から変換します。" : "最大解像度で取得してから、H.264/AACへ変換します。"}</small></div>
      <div class="setting-field"><div class="setting-label-row"><span class="setting-label">テーマ</span><button type="button" class="info-tip" aria-label="テーマの説明"><span class="info-glyph">i</span><span class="tooltip-bubble" role="tooltip">Light・Dark・自動から選べます。自動はOSの設定に合わせます。</span></button></div><select bind:value={settingsDraft.theme} onchange={(event) => applyTheme(event.currentTarget.value as Theme)}><option value="auto">自動（システム設定に合わせる）</option><option value="light">Light</option><option value="dark">Dark</option></select></div>

      <div class="generator">
        <div class="generator-heading"><div><div class="generator-title-row"><h3>Profile generator</h3><button type="button" class="info-tip" aria-label="Profile generatorの説明"><span class="info-glyph">i</span><span class="tooltip-bubble" role="tooltip">よく使う設定を選ぶだけで、yt-dlp用の引数を自動生成します。</span></button></div><p>よく使う設定からyt-dlp引数を作成</p></div><span class="sparkle">✦</span></div>
        <div class="generator-grid">
          <label>種類<select bind:value={generatorMode}><option value="video">動画</option><option value="audio">音声のみ</option></select></label>
          {#if generatorMode === "video"}<label>画質<select bind:value={generatorQuality}><option value="best">最高画質</option><option value="1080">1080p以下</option><option value="720">720p以下</option></select></label><label>形式<select bind:value={generatorContainer}><option value="mp4">MP4</option><option value="mkv">MKV</option><option value="webm">WebM</option></select></label>{:else}<label>音声形式<select bind:value={generatorAudioFormat}><option value="mp3">MP3</option><option value="m4a">M4A</option><option value="opus">Opus</option></select></label>{/if}
        </div>
        <div class="check-grid"><label class="check-row"><input type="checkbox" bind:checked={generatorMetadata} /> metadataを埋め込む</label><label class="check-row"><input type="checkbox" bind:checked={generatorThumbnail} /> サムネイルを埋め込む</label><label class="check-row"><input type="checkbox" bind:checked={generatorSubtitles} /> 字幕を保存する</label></div>
        <button class="generator-button" onclick={generateProfileArgs}>✦ 引数を生成してProfile argsに反映</button>
      </div>

      <div class="setting-field"><div class="setting-label-row"><span class="setting-label">Profile args</span><button type="button" class="info-tip" aria-label="Profile argsの説明"><span class="info-glyph">i</span><span class="tooltip-bubble" role="tooltip">1行に1引数です。ジェネレータで作成した後に手動編集もできます。</span></button></div><small>1行に1引数。ジェネレータで作成後、手動編集もできます。</small><textarea rows="6" value={settingsDraft.profile_args.join("\n")} oninput={(event) => settingsDraft.profile_args = event.currentTarget.value.split("\n")}></textarea></div>
      {#if hasToolIssue}<div class="tool-settings" bind:this={toolSettingsElement}><div class="tool-settings-heading"><div><h3>外部ツール</h3><p>yt-dlp・ffmpeg・Denoの接続状態</p></div><button class="secondary compact-button" onclick={refreshTools}>再チェック</button></div><div class="tool-grid"><div class="tool-check"><span>yt-dlp</span><strong class:missing={appState.yt_dlp_path === null}>{appState.yt_dlp_path ? "接続済み" : "未検出"}</strong></div><div class="tool-check"><span>ffmpeg</span><strong class:missing={appState.ffmpeg_path === null}>{appState.ffmpeg_path ? "接続済み" : "未検出"}</strong></div><div class="tool-check"><span>Deno <small>YouTube用</small></span><strong class:missing={appState.deno_path === null}>{appState.deno_path ? "接続済み" : "未検出"}</strong></div></div><p class="tool-help">インストール後、PATHに追加してから「再チェック」を押してください。</p><div class="tool-links"><button class="secondary" onclick={() => openExternal("https://github.com/yt-dlp/yt-dlp")}>yt-dlpを入手</button><button class="secondary" onclick={() => openExternal("https://ffmpeg.org/download.html")}>ffmpegを入手</button><button class="secondary" onclick={() => openExternal("https://docs.deno.com/runtime/getting_started/installation/")}>Denoを入手</button></div></div>{/if}
      <div class="modal-actions"><button class="secondary" onclick={closeSettings}>キャンセル</button><button class="primary" onclick={saveSettings}>保存</button></div>
    </dialog>
  </div>
{/if}
{#if selectedError}
  <div class="modal-backdrop" role="presentation" onclick={(event) => event.target === event.currentTarget && (selectedErrorId = null)}>
    <dialog open class="modal error-modal" aria-labelledby="error-log-title">
      <div class="modal-heading"><div><h2 id="error-log-title">エラーログ</h2><p>{shortTitle(selectedError)}</p></div><button class="remove-button" aria-label="閉じる" onclick={() => selectedErrorId = null}>×</button></div>
      <pre class="error-log">{errorLogText(selectedError)}</pre>
      <div class="modal-actions"><button class="secondary" onclick={() => copyErrorLog(selectedError)}>コピー</button><button class="primary" onclick={() => selectedErrorId = null}>閉じる</button></div>
    </dialog>
  </div>
{/if}
{#if toast}<div class="toast" role="status">{toast}</div>{/if}

<style>
  :global(*){box-sizing:border-box}:global(:root){font-family:Inter,ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;color:#25232b;background:#f8f7fb;font-synthesis:none;text-rendering:optimizeLegibility;-webkit-font-smoothing:antialiased}:global(body){margin:0;min-width:680px}:global(button),:global(input),:global(textarea){font:inherit}
  main{min-height:100vh;max-width:900px;margin:0 auto;padding:30px 44px 22px;transition:background .2s}main.drop-active{background:#eeebff}.topbar,.section-heading,.card-top,.metrics,.modal-heading,.modal-actions,.input-row,.brand,.switch-row,.topbar-actions,.tool-settings-heading{display:flex;align-items:center}.topbar{justify-content:space-between;margin-bottom:28px}.topbar-actions{gap:5px}.brand{gap:12px}.brand-mark,.download-symbol{display:grid;place-items:center;color:#fff;background:#6750a4;border-radius:14px;width:40px;height:40px;font-size:24px;font-weight:700;overflow:hidden}.brand-mark img{display:block;width:40px;height:40px}.brand h1{margin:0;font-size:19px;letter-spacing:-.02em}.brand p{margin:2px 0 0;color:#77737e;font-size:12px}.icon-button,.remove-button{border:0;background:transparent;cursor:pointer;color:#77737e}.icon-button{border-radius:50%;width:38px;height:38px;font-size:20px}.icon-button:hover,.remove-button:hover{color:#6750a4;background:#ece7f7}.profile-switcher{display:flex;align-items:center;gap:6px;max-width:180px;padding:7px 9px;border:1px solid transparent;border-radius:10px;color:#77737e;background:transparent;font-size:11px}.profile-switcher:hover,.profile-switcher:focus-visible{color:#6750a4;background:#eee9ff;border-color:#dfd5f4;outline:0}.profile-switcher strong{max-width:86px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:#514b59;font-size:11px}.profile-chevron{font-size:14px;line-height:1;color:#918c99}.tool-warning{display:grid;place-items:center;width:30px;height:30px;padding:0;border:0;border-radius:50%;color:#9a6a20;background:#fff4d6}.tool-warning:hover,.tool-warning:focus-visible{color:#7b5312;background:#ffe9ad;outline:0}.tool-warning svg{width:17px;height:17px;fill:none;stroke:currentColor;stroke-width:1.8;stroke-linecap:round;stroke-linejoin:round}
  h2{margin:0;font-size:15px;letter-spacing:-.01em}.drop-zone{padding:33px 28px 27px;text-align:center;border:1px dashed #b8aecf;border-radius:24px;background:#fff;box-shadow:0 3px 15px #39286d0b}.drop-icon{margin:auto;display:grid;place-items:center;width:42px;height:42px;border-radius:50%;color:#6750a4;background:#eee9ff;font-size:25px;font-weight:700}.drop-zone h2{margin-top:12px;font-size:18px}.drop-zone p{margin:5px 0 18px;font-size:13px;color:#77737e}.input-row{max-width:570px;margin:auto;gap:8px}.input-row input{min-width:0;flex:1;border:1px solid #dfdbe4;background:#fbfafc;border-radius:12px;padding:11px 14px;outline:0}.input-row input:focus{border-color:#6750a4;box-shadow:0 0 0 3px #6750a425}.drop-zone small{display:block;margin-top:12px;color:#918c99;font-size:11px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
  button{cursor:pointer;border:0}.primary,.secondary{border-radius:11px;padding:10px 17px;font-weight:650}.primary{color:#fff;background:#6750a4;box-shadow:0 2px 5px #6750a433}.primary:hover{background:#57428f}.secondary{color:#514b59;background:#eeeaf2}.section{margin-top:30px}.section-heading{justify-content:space-between;margin-bottom:12px}.section-heading>div{display:flex;align-items:baseline;gap:10px}.subheading{color:#918c99;font-size:11px}.count{display:grid;place-items:center;min-width:25px;height:25px;padding:0 7px;border-radius:99px;color:#6750a4;background:#eee9ff;font-size:12px;font-weight:700}.queue-heading-actions{display:flex;align-items:center}.queue-count-control{position:relative;display:block;flex:none;width:29px;height:29px}.queue-count-control .queue-count{position:absolute;inset:0;width:29px;min-width:29px;height:29px;padding:0;transition:opacity .12s ease}.clear-queue-button{position:absolute;inset:0;display:grid;place-items:center;width:29px;height:29px;padding:0;border-radius:50%;color:#a34558;background:transparent;opacity:0;pointer-events:none;transition:opacity .12s ease,background .15s ease}.queue-count-control.has-items:hover .queue-count,.queue-count-control.has-items:focus-within .queue-count{opacity:0}.queue-count-control.has-items:hover .clear-queue-button,.queue-count-control.has-items:focus-within .clear-queue-button,.clear-queue-button:focus-visible{opacity:1;pointer-events:auto}.clear-queue-button:hover{background:#f9e8ec}.clear-queue-button svg{width:17px;height:17px;fill:none;stroke:currentColor;stroke-width:1.8;stroke-linecap:round;stroke-linejoin:round}.download-card,.queue-list{background:#fff;border:1px solid #ebe7ef;border-radius:16px}.download-card{padding:17px;box-shadow:0 3px 14px #39286d08}.card-top{gap:10px}.download-symbol{flex:none;width:34px;height:34px;border-radius:11px;font-size:19px}.item-info,.queue-title{min-width:0;flex:1;display:grid;gap:3px}.item-info strong,.queue-title strong{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:13px}.item-info span,.queue-title span{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:#85808b;font-size:11px}.metrics{justify-content:space-between;color:#77737e;font-size:10px}.text-button{flex:none;padding:6px 8px;border-radius:8px;color:#6750a4;background:transparent;font-size:11px;font-weight:650}.text-button:hover{background:#f0ecf8}.text-button.danger{color:#a34558}.queue-list{overflow:hidden}.queue-row{min-height:56px;padding:8px 13px;display:flex;align-items:center;gap:10px;border-bottom:1px solid #f0edf2}.queue-row:last-child{border-bottom:0}.drag-handle{color:#a7a1ad;font-size:20px;cursor:grab}.remove-button{flex:none;border-radius:50%;width:29px;height:29px;font-size:21px;line-height:1}.empty-state{margin:0 0 5px;padding:28px;border:1px dashed #ded9e3;border-radius:15px;color:#99939f;text-align:center;font-size:13px}.empty-state.compact{padding:22px}
  .history-section{margin-top:28px}.history-row{padding-left:17px}.history-dot{flex:none;width:9px;height:9px;border-radius:50%;background:#9a929f}.history-dot.history-success{background:#5c9b72}.history-dot.history-failed{background:#b24d5e}
  .modal-backdrop{position:fixed;inset:0;display:grid;place-items:center;padding:20px;background:#241b3299;z-index:2}.modal{width:min(500px,100%);max-height:90vh;overflow:auto;padding:25px;border-radius:22px;background:#fff;box-shadow:0 20px 60px #241b3240}.modal-heading{justify-content:space-between;margin-bottom:22px}.modal-heading h2{font-size:20px}.modal-heading p{margin:4px 0 0;color:#85808b;font-size:12px}.modal>label{display:grid;gap:6px;margin:15px 0;color:#55505c;font-size:12px;font-weight:650}.modal>label.switch-row{display:flex;align-items:center;justify-content:space-between}.modal input:not(.switch):not([type="checkbox"]),.modal textarea{width:100%;padding:10px 12px;border:1px solid #dfdbe4;border-radius:10px;color:#292631;background:#fbfafc;outline:0;font-size:13px;font-weight:400}.modal textarea{resize:vertical;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12px}.modal input:focus,.modal textarea:focus{border-color:#6750a4}.switch-row span{display:grid;gap:4px}.switch{appearance:none;width:42px;height:24px;border-radius:20px;background:#d8d2dd;cursor:pointer;position:relative}.switch::after{content:"";position:absolute;top:3px;left:3px;width:18px;height:18px;border-radius:50%;background:#fff;transition:.18s;box-shadow:0 1px 3px #0003}.switch:checked{background:#6750a4}.switch:checked::after{transform:translateX(18px)}.modal-actions{justify-content:flex-end;gap:8px;margin-top:24px}.tool-settings{margin:20px 0;padding:15px;border:1px solid #e7e1eb;border-radius:15px;background:#fbfafc}.tool-settings-heading{justify-content:space-between;gap:12px;margin-bottom:12px}.tool-settings-heading h3{margin:0;color:#514b59;font-size:13px}.tool-settings-heading p{margin:4px 0 0;color:#85808b;font-size:11px}.compact-button{padding:7px 10px;font-size:11px}.tool-grid{display:grid;gap:9px;margin-bottom:16px}.tool-check{display:flex;justify-content:space-between;align-items:center;padding:12px 14px;border:1px solid #e7e1eb;border-radius:12px;background:#fbfafc;color:#514b59;font-size:13px}.tool-check strong{color:#5c9b72;font-size:12px}.tool-check strong.missing{color:#b24d5e}.tool-help{margin:0;color:#77737e;font-size:12px;line-height:1.6}.tool-links{display:flex;gap:8px;margin-top:16px;flex-wrap:wrap}.error-log{max-height:45vh;overflow:auto;margin:0;padding:14px;border:1px solid #e7e1eb;border-radius:12px;color:#3d3843;background:#fbfafc;font:12px/1.6 ui-monospace,SFMono-Regular,Menlo,monospace;white-space:pre-wrap;overflow-wrap:anywhere}.toast{position:fixed;right:24px;bottom:24px;max-width:380px;padding:12px 16px;border-radius:12px;color:#fff;background:#322d38;box-shadow:0 5px 20px #0003;font-size:12px;z-index:4}
  .generator{margin:22px 0;padding:16px;border:1px solid #e7e0f0;border-radius:15px;background:#faf8ff}.generator-heading{display:flex;align-items:flex-start;justify-content:space-between}.generator-heading h3{margin:0;color:#45376a;font-size:13px}.generator-title-row{display:flex;align-items:center;gap:7px}.generator-heading p{margin:4px 0 14px;color:#85808b;font-size:11px}.sparkle{color:#6750a4;font-size:18px}.setting-field{display:grid;gap:6px;margin:15px 0;color:#55505c;font-size:12px;font-weight:650}.setting-field>small{color:#85808b;font-size:11px;font-weight:400}.setting-label-row{display:flex;align-items:center;gap:7px}.setting-label{font-size:12px}.info-tip{position:relative;display:inline-grid;place-items:center;width:17px;height:17px;flex:none;padding:0;border:0;border-radius:50%;color:#6750a4;background:#eee9ff;cursor:help;outline:0}.info-glyph{font-size:11px;font-weight:750;line-height:1}.tooltip-bubble{position:absolute;z-index:5;top:calc(100% + 8px);left:50%;width:230px;padding:10px 11px;border:1px solid #dfd5f4;border-radius:10px;color:#514b59;background:#fff;box-shadow:0 6px 20px #39286d1c;font-size:11px;font-weight:400;line-height:1.5;opacity:0;visibility:hidden;pointer-events:none;transform:translate(-50%,-3px);transition:opacity .15s ease,transform .15s ease}.info-tip:hover .tooltip-bubble,.info-tip:focus-visible .tooltip-bubble{opacity:1;visibility:visible;transform:translate(-50%,0)}.generator-grid{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:9px}.generator-grid label{display:grid;gap:5px;min-width:0;color:#55505c;font-size:11px;font-weight:650}.modal select{width:100%;appearance:none;padding:9px 38px 9px 10px;border:1px solid #dfdbe4;border-radius:9px;color:#292631;background-color:#fff;background-image:url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='20' height='20' viewBox='0 0 24 24' fill='none' stroke='%235c5663' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'%3E%3Cpath d='m6 9 6 6 6-6'/%3E%3C/svg%3E");background-repeat:no-repeat;background-position:right 10px center;background-size:18px;outline:0;font-size:12px}.modal select:focus{border-color:#6750a4;box-shadow:0 0 0 3px #6750a425}.check-grid{display:grid;grid-template-columns:1fr;gap:8px;margin:15px 0}.check-row{display:flex!important;align-items:center;min-width:0;gap:8px;color:#625c69;font-size:11px;font-weight:400!important;white-space:normal;line-height:1.4}.check-row input{flex:0 0 auto;width:16px;height:16px;accent-color:#6750a4}.generator-button{width:100%;padding:9px;border-radius:9px;color:#6750a4;background:#eee9ff;font-size:11px;font-weight:700}.generator-button:hover{background:#e3dafc}
  @media (max-width:760px){:global(body){min-width:0}main{padding:22px 18px}}
  :global(body.theme-dark){color:#eeeaf2;background:#151318}
  :global(body.theme-dark) main.drop-active{background:#211b31}
  :global(body.theme-dark) .drop-zone,:global(body.theme-dark) .download-card,:global(body.theme-dark) .queue-list,:global(body.theme-dark) .modal{color:#eeeaf2;background:#211f25;border-color:#45404c;box-shadow:0 3px 18px #0004}
  :global(body.theme-dark) .drop-zone p,:global(body.theme-dark) .brand p,:global(body.theme-dark) .item-info span,:global(body.theme-dark) .queue-title span,:global(body.theme-dark) .subheading,:global(body.theme-dark) .drop-zone small,:global(body.theme-dark) .metrics,:global(body.theme-dark) .modal-heading p{color:#aaa4b0}
  :global(body.theme-dark) .drop-zone h2,:global(body.theme-dark) h1,:global(body.theme-dark) h2,:global(body.theme-dark) h3,:global(body.theme-dark) strong,:global(body.theme-dark) .modal>label{color:#eeeaf2}
  :global(body.theme-dark) .input-row input,:global(body.theme-dark) .modal input:not(.switch):not([type="checkbox"]),:global(body.theme-dark) .modal textarea{color:#eeeaf2;background-color:#2b2830;border-color:#4c4654}
  :global(body.theme-dark) .modal select{color:#eeeaf2;background-color:#2b2830;border-color:#4c4654;background-image:url("data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='20' height='20' viewBox='0 0 24 24' fill='none' stroke='%23eeeaf2' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'%3E%3Cpath d='m6 9 6 6 6-6'/%3E%3C/svg%3E")}
  :global(body.theme-dark) .input-row input::placeholder,:global(body.theme-dark) .modal input::placeholder{color:#8e8795}
  :global(body.theme-dark) .empty-state{color:#aaa4b0;border-color:#4c4654}
  :global(body.theme-dark) .generator{background:#27222f;border-color:#514663}
  :global(body.theme-dark) .generator-heading h3{color:#d8c8ff}:global(body.theme-dark) .generator-heading p{color:#aaa4b0}:global(body.theme-dark) .check-row{color:#c0bac5}
  :global(body.theme-dark) .clear-queue-button:hover{background:#4b2d38}
  :global(body.theme-dark) .secondary{color:#e4dfea;background:#413b48}:global(body.theme-dark) .icon-button,:global(body.theme-dark) .remove-button{color:#aaa4b0}:global(body.theme-dark) .icon-button:hover,:global(body.theme-dark) .remove-button:hover{color:#d3baff;background:#3b304f}:global(body.theme-dark) .info-tip{color:#d3baff;background:#3d3157}:global(body.theme-dark) .tooltip-bubble{border-color:#514663;color:#eeeaf2;background:#2b2830;box-shadow:0 6px 20px #0006}
  :global(body.theme-dark) .profile-switcher strong{color:#eeeaf2}:global(body.theme-dark) .profile-switcher:hover,:global(body.theme-dark) .profile-switcher:focus-visible{color:#d3baff;background:#382d4c;border-color:#514663}:global(body.theme-dark) .tool-warning{color:#f6cf73;background:#493a20}:global(body.theme-dark) .tool-warning:hover,:global(body.theme-dark) .tool-warning:focus-visible{color:#ffe49a;background:#604b25}:global(body.theme-dark) .history-dot{background:#756d7a}:global(body.theme-dark) .history-dot.history-success{background:#77b88c}:global(body.theme-dark) .history-dot.history-failed{background:#d17a88}
  :global(body.theme-dark) .modal-backdrop{background:#0009}:global(body.theme-dark) .tool-settings{border-color:#4c4654;background:#27242b}:global(body.theme-dark) .tool-settings-heading h3{color:#eeeaf2}:global(body.theme-dark) .tool-settings-heading p{color:#aaa4b0}:global(body.theme-dark) .tool-check{border-color:#4c4654;background:#2b2830;color:#eeeaf2}:global(body.theme-dark) .tool-help{color:#aaa4b0}:global(body.theme-dark) .error-log{border-color:#4c4654;color:#eeeaf2;background:#2b2830}:global(body.theme-dark) .toast{background:#f0eaf8;color:#30283b}
  .download-list{display:grid;gap:8px}.download-list .queue-row{background:#fff;border:1px solid #ebe7ef;border-radius:16px}.download-list .queue-row:last-child{border-bottom:1px solid #ebe7ef}.download-card{position:relative;overflow:hidden}.download-card>*:not(.download-progress-fill){position:relative;z-index:1}.download-progress-fill{position:absolute;inset:0 auto 0 0;width:var(--progress);background:#eee9ff;opacity:.9;transition:width .25s ease;pointer-events:none}.download-card.paused .download-progress-fill{background:#e7e0f4}.progress-percent{color:#6750a4;font-size:11px}
  :global(body.theme-dark) .download-list .queue-row{background:#211f25;border-color:#45404c}:global(body.theme-dark) .download-list .queue-row:last-child{border-bottom-color:#45404c}:global(body.theme-dark) .download-progress-fill{background:#3d3157}:global(body.theme-dark) .download-card.paused .download-progress-fill{background:#37303f}:global(body.theme-dark) .progress-percent{color:#d3baff}
</style>
