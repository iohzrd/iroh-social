<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { getCurrentWebview } from "@tauri-apps/api/webview";
  import { readImage } from "@tauri-apps/plugin-clipboard-manager";
  import { platform } from "@tauri-apps/plugin-os";
  import MentionAutocomplete from "$lib/MentionAutocomplete.svelte";
  import { isImage, isVideo } from "$lib/utils";
  import type { PendingAttachment } from "$lib/types";
  import {
    useMentionAutocomplete,
    useFileUpload,
    autogrow,
  } from "$lib/composables";
  import { onMount } from "svelte";

  const MAX_POST_LENGTH = 10_000;
  const isMobile = platform() === "android" || platform() === "ios";

  let {
    pubkey,
    onsubmitted,
  }: {
    pubkey: string;
    onsubmitted: () => void;
  } = $props();

  let newPost = $state("");
  let posting = $state(false);
  let dragging = $state(false);
  let fileInput = $state<HTMLInputElement>(null!);
  let cameraInput = $state<HTMLInputElement>(null!);
  let mentionAutocomplete = $state<MentionAutocomplete>();

  const mention = useMentionAutocomplete(
    () => newPost,
    (v) => (newPost = v),
    ".compose textarea",
  );

  const upload = useFileUpload();

  onMount(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;

    getCurrentWebview()
      .onDragDropEvent(async (event) => {
        if (cancelled) return;
        const { type } = event.payload;
        if (type === "enter" || type === "over") {
          dragging = true;
        } else if (type === "leave") {
          dragging = false;
        } else if (type === "drop") {
          dragging = false;
          const paths: string[] = (
            event.payload as { type: string; paths: string[] }
          ).paths;
          if (paths.length > 0) {
            await upload.addFilesFromPaths(paths);
          }
        }
      })
      .then((fn) => {
        if (cancelled) {
          fn();
        } else {
          unlisten = fn;
        }
      });

    return () => {
      cancelled = true;
      unlisten?.();
    };
  });

  async function submitPost() {
    if ((!newPost.trim() && upload.attachments.length === 0) || posting) return;
    posting = true;
    try {
      const media = upload.attachments.map(
        ({ hash, ticket, mime_type, filename, size }) => ({
          hash,
          ticket,
          mime_type,
          filename,
          size,
        }),
      );
      await invoke("create_post", {
        content: newPost,
        media: media.length > 0 ? media : null,
      });
      newPost = "";
      upload.clear();
      onsubmitted();
    } catch (e) {
      console.error("Failed to create post:", e);
    }
    posting = false;
  }

  async function handlePaste(e: ClipboardEvent) {
    if (isMobile) return;
    try {
      const img = await readImage();
      const rgba = await img.rgba();
      const { width, height } = await img.size();
      await img.close();
      if (rgba.length === 0) return;
      e.preventDefault();
      await upload.addImageFromRgba(rgba, width, height);
    } catch {
      // No image in clipboard — let the default text paste through.
    }
  }

  function handleKey(e: KeyboardEvent) {
    if (mentionAutocomplete?.handleKey(e)) return;
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      submitPost();
    }
  }
</script>

<div class="compose" class:drag-over={dragging}>
  <MentionAutocomplete
    bind:this={mentionAutocomplete}
    query={mention.query}
    selfId={pubkey}
    visible={mention.active}
    onselect={mention.insertMention}
  />
  <textarea
    class="input-base compose-textarea"
    bind:value={newPost}
    placeholder="What's on your mind?"
    rows="1"
    maxlength={MAX_POST_LENGTH}
    onkeydown={handleKey}
    oninput={mention.handleInput}
    onpaste={handlePaste}
    use:autogrow></textarea>
  <div class="compose-meta">
    <span class="hint">Shift+Enter for newline</span>
    <span
      class="char-count"
      class:warn={newPost.length > MAX_POST_LENGTH * 0.9}
    >
      {newPost.length}/{MAX_POST_LENGTH}
    </span>
  </div>

  {#if upload.attachments.length > 0}
    <div class="attachment-previews">
      {#each upload.attachments as att, i}
        <div class="attachment-preview">
          {#if isImage(att.mime_type)}
            <img src={att.previewUrl} alt={att.filename} />
          {:else if isVideo(att.mime_type)}
            <video src={att.previewUrl} muted></video>
          {:else}
            <div class="file-icon">{att.filename}</div>
          {/if}
          <button
            class="remove-btn"
            onclick={() => upload.removeAttachment(i)}
            aria-label="Remove attachment">&times;</button
          >
        </div>
      {/each}
    </div>
  {/if}

  {#if upload.errorMessage}
    <p class="compose-error">{upload.errorMessage}</p>
  {/if}

  <div class="compose-actions">
    {#if isMobile}
      <button
        class="btn-elevated attach-btn"
        onclick={() => cameraInput.click()}
        disabled={upload.uploading}
      >
        {upload.uploading ? "..." : "Camera"}
      </button>
      <button
        class="btn-elevated attach-btn"
        onclick={() => fileInput.click()}
        disabled={upload.uploading}
      >
        {upload.uploading ? "..." : "Gallery"}
      </button>
    {:else}
      <button
        class="btn-elevated attach-btn"
        onclick={() => fileInput.click()}
        disabled={upload.uploading}
      >
        {upload.uploading ? "Uploading..." : "Attach"}
      </button>
    {/if}
    <input
      bind:this={cameraInput}
      type="file"
      accept="image/*,video/*"
      capture="environment"
      onchange={upload.handleFiles}
      hidden
    />
    <input
      bind:this={fileInput}
      type="file"
      multiple
      accept="image/*,video/*,audio/*,.pdf,.txt"
      onchange={upload.handleFiles}
      hidden
    />
    <button
      class="btn-accent post-btn"
      onclick={submitPost}
      disabled={posting || (!newPost.trim() && upload.attachments.length === 0)}
    >
      {posting ? "Posting..." : "Post"}
    </button>
  </div>
</div>

<style>
  .compose {
    position: relative;
    margin-bottom: 1.25rem;
    border: 2px solid transparent;
    border-radius: var(--radius-xl);
    transition: border-color 0.15s;
  }

  .compose.drag-over {
    border-color: var(--color-accent);
  }

  .compose-textarea {
    border-radius: var(--radius-xl);
    padding: 0.75rem;
    font-size: var(--text-lg);
    resize: none;
    overflow: hidden;
  }

  .compose-meta {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-top: 0.25rem;
  }

  .hint {
    font-size: var(--text-sm);
    color: var(--text-muted);
  }

  .char-count {
    font-size: var(--text-sm);
    color: var(--text-muted);
  }

  .char-count.warn {
    color: var(--color-warning);
  }

  .compose-actions {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.5rem;
  }

  .attach-btn {
    border-radius: var(--radius-lg);
    padding: 0.55rem 1rem;
    font-size: var(--text-base);
    font-weight: 500;
  }

  .post-btn {
    flex: 1;
    border-radius: var(--radius-lg);
    padding: 0.55rem;
    font-size: var(--text-base);
  }

  .attachment-previews {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.5rem;
    flex-wrap: wrap;
  }

  .attachment-preview {
    position: relative;
    width: 80px;
    height: 80px;
    border-radius: var(--radius-md);
    overflow: hidden;
    border: 1px solid var(--border);
  }

  .attachment-preview img,
  .attachment-preview video {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }

  .attachment-preview .file-icon {
    width: 100%;
    height: 100%;
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--bg-deep);
    color: var(--text-secondary);
    font-size: var(--text-xs);
    text-align: center;
    padding: 0.25rem;
    word-break: break-all;
  }

  .remove-btn {
    position: absolute;
    top: -4px;
    right: -4px;
    width: 28px;
    height: 28px;
    border-radius: 50%;
    background: var(--overlay-medium);
    color: var(--text-on-accent);
    border: none;
    font-size: var(--text-base);
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    line-height: 1;
  }

  .compose-error {
    color: var(--color-error-light);
    font-size: var(--text-sm);
    margin: 0.25rem 0 0;
  }
</style>
