<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import MentionAutocomplete from "$lib/MentionAutocomplete.svelte";
  import { useMentionAutocomplete, autogrow } from "$lib/composables";

  let {
    replyToId,
    replyToAuthor,
    pubkey,
    onsubmitted,
    oncancel,
  }: {
    replyToId: string;
    replyToAuthor: string;
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
    ".reply-composer textarea",
  );

  async function submit() {
    if (!content.trim() || posting) return;
    posting = true;
    try {
      await invoke("create_post", {
        content: content.trim(),
        media: null,
        replyTo: replyToId,
        replyToAuthor: replyToAuthor,
      });
      content = "";
      onsubmitted?.();
    } catch (e) {
      console.error("Failed to post reply:", e);
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

<div class="composer reply-composer">
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
    placeholder="Write a reply..."
    rows="1"
    onkeydown={handleKey}
    oninput={mention.handleInput}
    use:autogrow></textarea>
  <div class="composer-actions">
    <button class="btn-cancel" onclick={oncancel}>Cancel</button>
    <button
      class="btn-accent composer-submit"
      onclick={submit}
      disabled={posting || !content.trim()}
    >
      {posting ? "Posting..." : "Reply"}
    </button>
  </div>
</div>
