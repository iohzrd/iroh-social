<script lang="ts">
  import { platform } from "@tauri-apps/plugin-os";
  import type { PendingAttachment, MediaAttachment } from "$lib/types";
  import { isImage, uploadFiles } from "$lib/utils";
  import { autogrow } from "$lib/composables";

  let {
    onsubmit,
    oninput,
  }: {
    onsubmit: (text: string, media: MediaAttachment[] | null) => Promise<void>;
    oninput?: () => void;
  } = $props();

  const isMobile = platform() === "android" || platform() === "ios";
  let messageText = $state("");
  let sending = $state(false);
  let sendError = $state("");
  let attachments = $state<PendingAttachment[]>([]);
  let uploading = $state(false);
  let fileInput = $state<HTMLInputElement>(null!);
  let cameraInput = $state<HTMLInputElement>(null!);

  async function handleFiles(e: Event) {
    const input = e.target as HTMLInputElement;
    const files = input.files;
    if (!files || files.length === 0) return;
    uploading = true;
    try {
      const uploaded = await uploadFiles(files);
      attachments = [...attachments, ...uploaded];
    } catch (e) {
      console.error("Failed to upload files:", e);
    }
    uploading = false;
    input.value = "";
  }

  function removeAttachment(index: number) {
    const removed = attachments[index];
    if (removed) URL.revokeObjectURL(removed.previewUrl);
    attachments = attachments.filter((_, i) => i !== index);
  }

  async function send() {
    const text = messageText.trim();
    if ((!text && attachments.length === 0) || sending) return;
    sending = true;
    sendError = "";
    try {
      const media =
        attachments.length > 0
          ? attachments.map(({ hash, ticket, mime_type, filename, size }) => ({
              hash,
              ticket,
              mime_type,
              filename,
              size,
            }))
          : null;
      await onsubmit(text, media);
      messageText = "";
      for (const a of attachments) URL.revokeObjectURL(a.previewUrl);
      attachments = [];
    } catch (e) {
      console.error("Failed to send message:", e);
      sendError = String(e);
      setTimeout(() => (sendError = ""), 5000);
    }
    sending = false;
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  }

  function handleInput() {
    oninput?.();
  }

  export function revokeAttachments() {
    for (const a of attachments) URL.revokeObjectURL(a.previewUrl);
  }
</script>

<input
  type="file"
  multiple
  class="hidden-input"
  bind:this={fileInput}
  onchange={handleFiles}
/>
<input
  type="file"
  accept="image/*,video/*"
  capture="environment"
  class="hidden-input"
  bind:this={cameraInput}
  onchange={handleFiles}
/>

{#if sendError}
  <div class="send-error">{sendError}</div>
{/if}

{#if attachments.length > 0}
  <div class="attachment-preview">
    {#each attachments as att, i}
      <div class="attachment-item">
        {#if isImage(att.mime_type)}
          <img src={att.previewUrl} alt={att.filename} />
        {:else}
          <span class="att-file">{att.filename}</span>
        {/if}
        <button
          class="att-remove"
          onclick={() => removeAttachment(i)}
          aria-label="Remove attachment">x</button
        >
      </div>
    {/each}
  </div>
{/if}

<div class="compose-bar">
  {#if isMobile}
    <button
      class="attach-btn"
      onclick={() => cameraInput?.click()}
      disabled={uploading}
      title="Take photo"
    >
      {uploading ? "..." : "Cam"}
    </button>
  {/if}
  <button
    class="attach-btn"
    onclick={() => fileInput?.click()}
    disabled={uploading}
    title="Attach file"
  >
    {uploading ? "..." : "+"}
  </button>
  <textarea
    class="input-base compose-input"
    placeholder="Type a message..."
    bind:value={messageText}
    onkeydown={handleKeydown}
    oninput={handleInput}
    rows="1"
    use:autogrow></textarea>
  <button
    class="btn-accent send-btn"
    onclick={send}
    disabled={(!messageText.trim() && attachments.length === 0) || sending}
  >
    {sending ? "..." : "Send"}
  </button>
</div>

<style>
  .send-error {
    background: var(--color-error-light-bg);
    color: var(--color-error-light);
    font-size: var(--text-base);
    padding: 0.4rem 1rem;
    border-top: 1px solid var(--color-error-light-border);
  }

  .attachment-preview {
    display: flex;
    gap: 0.5rem;
    padding: 0.5rem 1rem;
    border-top: 1px solid var(--border);
    background: var(--bg-surface);
    overflow-x: auto;
    flex-shrink: 0;
  }

  .attachment-item {
    position: relative;
    flex-shrink: 0;
  }

  .attachment-item img {
    width: 60px;
    height: 60px;
    object-fit: cover;
    border-radius: var(--radius-md);
    border: 1px solid var(--border);
  }

  .att-file {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 60px;
    height: 60px;
    background: var(--bg-elevated);
    border-radius: var(--radius-md);
    font-size: var(--text-xs);
    color: var(--text-secondary);
    text-align: center;
    padding: 4px;
    word-break: break-all;
    overflow: hidden;
  }

  .att-remove {
    position: absolute;
    top: -6px;
    right: -6px;
    width: 28px;
    height: 28px;
    border-radius: 50%;
    background: var(--color-error-light);
    color: var(--text-on-accent);
    border: none;
    font-size: var(--text-sm);
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    line-height: 1;
  }

  .compose-bar {
    display: flex;
    align-items: flex-end;
    gap: 0.5rem;
    padding: 0.75rem 1rem;
    padding-bottom: calc(0.75rem + env(safe-area-inset-bottom));
    border-top: 1px solid var(--border);
    background: var(--bg-base);
    flex-shrink: 0;
  }

  .attach-btn {
    background: none;
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    color: var(--accent-medium);
    font-size: var(--text-xl);
    width: 36px;
    height: 36px;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
  }

  .attach-btn:hover:not(:disabled) {
    background: var(--bg-elevated);
    color: var(--accent-light);
  }

  .compose-input {
    flex: 1;
    border-radius: var(--radius-lg);
    font-size: var(--text-base);
    resize: none;
    min-height: 36px;
    max-height: 40vh;
  }

  .send-btn {
    border-radius: var(--radius-lg);
  }
</style>
