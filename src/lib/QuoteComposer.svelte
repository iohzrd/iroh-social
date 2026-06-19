<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import type { Post } from "$lib/types";
  import { shortId, getDisplayName } from "$lib/utils";
  import MentionAutocomplete from "$lib/MentionAutocomplete.svelte";
  import { useMentionAutocomplete, autogrow } from "$lib/composables";

  let {
    quotedPost,
    pubkey,
    onsubmitted,
    oncancel,
  }: {
    quotedPost: Post;
    pubkey: string;
    onsubmitted?: () => void;
    oncancel?: () => void;
  } = $props();

  let content = $state("");
  let posting = $state(false);
  let mentionAutocomplete: MentionAutocomplete;

  const mention = useMentionAutocomplete(
    () => content,
    (v) => (content = v),
    ".quote-composer textarea",
  );

  let preview = $derived(
    quotedPost.content.length > 120
      ? quotedPost.content.slice(0, 120) + "..."
      : quotedPost.content,
  );

  async function submit() {
    if (posting) return;
    posting = true;
    try {
      await invoke("create_post", {
        content: content.trim(),
        media: null,
        replyTo: null,
        replyToAuthor: null,
        quoteOf: quotedPost.id,
        quoteOfAuthor: quotedPost.author,
      });
      content = "";
      onsubmitted?.();
    } catch (e) {
      console.error("Failed to post quote:", e);
    }
    posting = false;
  }

  function handleKey(e: KeyboardEvent) {
    if (mentionAutocomplete?.handleKey(e)) return;
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      submit();
    } else if (e.key === "Escape") {
      oncancel?.();
    }
  }
</script>

<div class="composer quote-composer">
  <div class="quoted-preview">
    {#await getDisplayName(quotedPost.author, pubkey)}
      <span class="quote-author">{shortId(quotedPost.author)}</span>
    {:then name}
      <span class="quote-author">{name}</span>
    {/await}
    {#if preview}
      <span class="quote-text">{preview}</span>
    {:else}
      <span class="quote-text empty">[no text]</span>
    {/if}
  </div>
  <MentionAutocomplete
    bind:this={mentionAutocomplete}
    query={mention.query}
    selfId={pubkey}
    visible={mention.active}
    onselect={mention.insertMention}
  />
  <textarea
    class="textarea-base"
    bind:value={content}
    placeholder="Add your commentary (optional)..."
    rows="1"
    onkeydown={handleKey}
    oninput={mention.handleInput}
    use:autogrow></textarea>
  <div class="composer-actions">
    <button class="btn-cancel" onclick={oncancel}>Cancel</button>
    <button
      class="btn-accent composer-submit"
      onclick={submit}
      disabled={posting}
    >
      {posting ? "Posting..." : "Quote"}
    </button>
  </div>
</div>

<style>
  .quoted-preview {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    padding: 0.5rem 0.7rem;
    margin-bottom: 0.5rem;
    background: var(--bg-deep);
    border-left: 2px solid var(--accent);
    border-radius: 0 var(--radius-md) var(--radius-md) 0;
    font-size: var(--text-base);
  }

  .quote-author {
    color: var(--accent-light);
    font-weight: 600;
    font-size: var(--text-sm);
  }

  .quote-text {
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .quote-text.empty {
    font-style: italic;
    color: var(--text-muted);
  }
</style>
