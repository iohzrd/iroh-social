<script lang="ts">
  import { goto } from "$app/navigation";
  import { invoke } from "@tauri-apps/api/core";
  import { detectImageMime } from "$lib/utils";
  import type { Visibility } from "$lib/types";

  let {
    onback,
  }: {
    onback: () => void;
  } = $props();

  let displayName = $state("");
  let bio = $state("");
  let visibility = $state<Visibility>("public");
  let avatarPreview = $state<string | null>(null);
  let avatarHash = $state<string | null>(null);
  let avatarTicket = $state<string | null>(null);
  let saving = $state(false);
  let uploading = $state(false);
  let fileInput = $state<HTMLInputElement>(null!);

  const visibilityOptions: {
    value: Visibility;
    label: string;
    hint: string;
  }[] = [
    {
      value: "public",
      label: "Public",
      hint: "Anyone can see your posts and sync your profile.",
    },
    {
      value: "listed",
      label: "Listed",
      hint: "Profile discoverable, but posts only shared with approved followers.",
    },
    {
      value: "private",
      label: "Private",
      hint: "Only mutual follows can see your posts. Invisible to servers.",
    },
  ];

  async function handleAvatarUpload(e: Event) {
    const input = e.target as HTMLInputElement;
    const file = input.files?.[0];
    if (!file) return;
    uploading = true;
    try {
      const buffer = await file.arrayBuffer();
      const data = Array.from(new Uint8Array(buffer));
      const result: { hash: string; ticket: string } = await invoke(
        "add_blob_bytes",
        { data },
      );
      avatarHash = result.hash;
      avatarTicket = result.ticket;
      const blob = new Blob([new Uint8Array(buffer)], {
        type: detectImageMime(new Uint8Array(buffer)),
      });
      avatarPreview = URL.createObjectURL(blob);
    } catch (e) {
      console.error("Failed to upload avatar:", e);
    }
    uploading = false;
    input.value = "";
  }

  async function saveProfile() {
    if (!displayName.trim()) return;
    saving = true;
    try {
      await invoke("save_my_profile", {
        displayName: displayName.trim(),
        bio: bio.trim(),
        avatarHash,
        avatarTicket,
        visibility,
      });
      goto("/");
    } catch (e) {
      console.error("Failed to save profile:", e);
    }
    saving = false;
  }
</script>

<input
  type="file"
  accept="image/*"
  class="hidden-input"
  bind:this={fileInput}
  onchange={handleAvatarUpload}
/>

<div class="step">
  <h2>Create Your Profile</h2>

  <div class="avatar-section">
    <button
      class="avatar-upload"
      onclick={() => fileInput?.click()}
      disabled={uploading}
    >
      {#if avatarPreview}
        <img src={avatarPreview} alt="Avatar" />
      {:else}
        <span class="avatar-placeholder">
          {uploading ? "..." : "+"}
        </span>
      {/if}
    </button>
    <span class="avatar-hint">Add a photo</span>
  </div>

  <label class="field">
    <span class="field-label">Display Name</span>
    <input
      class="input-base"
      type="text"
      bind:value={displayName}
      placeholder="Your name"
      maxlength="50"
    />
  </label>

  <label class="field">
    <span class="field-label">Bio</span>
    <textarea
      class="input-base"
      bind:value={bio}
      placeholder="Tell people about yourself (optional)"
      rows="3"
      maxlength="300"></textarea>
  </label>

  <div class="field">
    <span class="field-label">Visibility</span>
    <div class="visibility-options">
      {#each visibilityOptions as opt}
        <label
          class="visibility-option"
          class:selected={visibility === opt.value}
        >
          <input
            type="radio"
            name="visibility"
            value={opt.value}
            bind:group={visibility}
          />
          <span class="visibility-label">{opt.label}</span>
          <span class="visibility-hint">{opt.hint}</span>
        </label>
      {/each}
    </div>
  </div>

  <div class="actions">
    <button class="secondary-btn" onclick={onback}>Back</button>
    <button
      class="btn-accent primary-btn"
      onclick={saveProfile}
      disabled={!displayName.trim() || saving}
    >
      {saving ? "Saving..." : "Get Started"}
    </button>
  </div>
</div>

<style>
  .step {
    max-width: 420px;
    width: 100%;
    text-align: center;
  }

  h2 {
    font-size: var(--text-2xl);
    color: var(--text-primary);
    margin: 0 0 1.5rem;
  }

  .avatar-section {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.5rem;
    margin-bottom: 1.5rem;
  }

  .avatar-upload {
    width: 80px;
    height: 80px;
    border-radius: 50%;
    border: 2px dashed var(--border-hover);
    background: var(--bg-surface);
    cursor: pointer;
    overflow: hidden;
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .avatar-upload:hover {
    border-color: var(--accent);
  }

  .avatar-upload img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }

  .avatar-placeholder {
    color: var(--text-tertiary);
    font-size: var(--text-icon-xl);
  }

  .avatar-hint {
    color: var(--text-tertiary);
    font-size: var(--text-sm);
  }

  .field {
    display: block;
    text-align: left;
  }

  .field textarea {
    resize: vertical;
    min-height: 60px;
  }

  .actions {
    display: flex;
    gap: 0.75rem;
    margin-top: 1.5rem;
  }

  .primary-btn {
    flex: 1;
    border-radius: var(--radius-lg);
    padding: 0.7rem 1.5rem;
    font-size: var(--text-lg);
  }

  .secondary-btn {
    background: none;
    border: 1px solid var(--border-hover);
    color: var(--text-secondary);
    border-radius: var(--radius-lg);
    padding: 0.7rem 1.5rem;
    font-size: var(--text-lg);
    cursor: pointer;
    transition:
      color var(--transition-fast),
      border-color var(--transition-fast);
  }

  .secondary-btn:hover {
    color: var(--accent-light);
    border-color: var(--accent);
  }

  .visibility-options {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }

  .visibility-option {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.5rem;
    padding: 0.6rem 0.75rem;
    background: var(--bg-surface);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    cursor: pointer;
    text-align: left;
    transition:
      border-color var(--transition-normal),
      background var(--transition-normal);
  }

  .visibility-option:hover {
    border-color: var(--border-hover);
  }

  .visibility-option.selected {
    border-color: var(--accent);
    background: var(--bg-elevated);
  }

  .visibility-option input[type="radio"] {
    accent-color: var(--accent);
    margin: 0;
  }

  .visibility-label {
    font-size: var(--text-base);
    font-weight: 600;
    color: var(--text-primary);
  }

  .visibility-hint {
    width: 100%;
    font-size: var(--text-sm);
    color: var(--text-tertiary);
    padding-left: 1.5rem;
  }
</style>
