<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount } from "svelte";
  import type { Profile, Visibility } from "$lib/types";
  import { avatarColor, getInitials, detectImageMime } from "$lib/utils";

  let {
    pubkey,
    profile,
    onsaved,
    oncancel,
  }: {
    pubkey: string;
    profile: Profile;
    onsaved: () => void;
    oncancel: () => void;
  } = $props();

  let editDisplayName = $state(profile.display_name);
  let editBio = $state(profile.bio);
  let editAvatarHash = $state<string | null>(profile.avatar_hash);
  let editAvatarTicket = $state<string | null>(profile.avatar_ticket);
  let editAvatarPreview = $state<string | null>(null);
  let editVisibility = $state<Visibility>(profile.visibility);
  let savedDisplayName = profile.display_name;
  let savedBio = profile.bio;
  let savedAvatarHash = profile.avatar_hash;
  let savedVisibility = profile.visibility;
  let saving = $state(false);
  let uploading = $state(false);
  let fileInput = $state<HTMLInputElement>(null!);
  let errorMessage = $state("");

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
      hint: "Your profile is discoverable, but posts are only shared with approved followers.",
    },
    {
      value: "private",
      label: "Private",
      hint: "Only mutual follows can see your posts. You are invisible to servers.",
    },
  ];

  let isDirty = $derived(
    editDisplayName !== savedDisplayName ||
      editBio !== savedBio ||
      editAvatarHash !== savedAvatarHash ||
      editVisibility !== savedVisibility,
  );

  async function loadAvatarPreview(ticket: string) {
    try {
      const bytes: number[] = await invoke("fetch_blob_bytes", { ticket });
      const data = new Uint8Array(bytes);
      const blob = new Blob([data], { type: detectImageMime(data) });
      if (editAvatarPreview) URL.revokeObjectURL(editAvatarPreview);
      editAvatarPreview = URL.createObjectURL(blob);
    } catch (e) {
      console.error("Failed to load avatar:", e);
    }
  }

  async function handleAvatarFile(e: Event) {
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
      editAvatarHash = result.hash;
      editAvatarTicket = result.ticket;
      if (editAvatarPreview) URL.revokeObjectURL(editAvatarPreview);
      editAvatarPreview = URL.createObjectURL(file);
    } catch (err) {
      errorMessage = `Upload failed: ${err}`;
      setTimeout(() => (errorMessage = ""), 4000);
    }
    uploading = false;
    input.value = "";
  }

  function removeAvatar() {
    editAvatarHash = null;
    editAvatarTicket = null;
    if (editAvatarPreview) {
      URL.revokeObjectURL(editAvatarPreview);
      editAvatarPreview = null;
    }
  }

  async function saveProfile() {
    saving = true;
    editDisplayName = editDisplayName.trim();
    editBio = editBio.trim();
    try {
      await invoke("save_my_profile", {
        displayName: editDisplayName,
        bio: editBio,
        avatarHash: editAvatarHash,
        avatarTicket: editAvatarTicket,
        visibility: editVisibility,
      });
      onsaved();
    } catch (err) {
      errorMessage = `Error: ${err}`;
      setTimeout(() => (errorMessage = ""), 4000);
    }
    saving = false;
  }

  onMount(() => {
    if (profile.avatar_ticket) {
      loadAvatarPreview(profile.avatar_ticket);
    }
    return () => {
      if (editAvatarPreview) URL.revokeObjectURL(editAvatarPreview);
    };
  });
</script>

<h2 class="edit-heading">Edit Profile</h2>
<div class="edit-form">
  <div class="field">
    <span class="field-label">Avatar</span>
    <div class="avatar-row">
      {#if editAvatarPreview}
        <img src={editAvatarPreview} alt="Avatar" class="avatar-edit-preview" />
      {:else}
        <div class="avatar-fallback" style="background:{avatarColor(pubkey)}">
          {getInitials(editDisplayName || "You", !editDisplayName)}
        </div>
      {/if}
      <div class="avatar-actions">
        <button
          class="avatar-btn"
          onclick={() => fileInput.click()}
          disabled={uploading}
        >
          {uploading ? "Uploading..." : editAvatarHash ? "Change" : "Upload"}
        </button>
        {#if editAvatarHash}
          <button class="avatar-btn remove" onclick={removeAvatar}
            >Remove</button
          >
        {/if}
      </div>
      <input
        bind:this={fileInput}
        type="file"
        accept="image/*"
        onchange={handleAvatarFile}
        hidden
      />
    </div>
  </div>

  <div class="field">
    <span class="field-label">Display Name</span>
    <input bind:value={editDisplayName} placeholder="Anonymous" />
  </div>

  <div class="field">
    <span class="field-label">Bio</span>
    <textarea
      bind:value={editBio}
      placeholder="Tell the world about yourself..."
      rows="3"></textarea>
  </div>

  <div class="field">
    <span class="field-label">Visibility</span>
    <div class="visibility-options">
      {#each visibilityOptions as opt}
        <label
          class="visibility-option"
          class:selected={editVisibility === opt.value}
        >
          <input
            type="radio"
            name="visibility"
            value={opt.value}
            bind:group={editVisibility}
          />
          <span class="visibility-label">{opt.label}</span>
          <span class="visibility-hint">{opt.hint}</span>
        </label>
      {/each}
    </div>
  </div>

  {#if errorMessage}
    <p class="edit-error">{errorMessage}</p>
  {/if}

  <div class="edit-actions">
    <button class="btn-cancel cancel-btn" onclick={oncancel}>Cancel</button>
    <button
      class="btn-accent save-btn"
      onclick={saveProfile}
      disabled={saving || !isDirty}
    >
      {saving ? "Saving..." : "Save"}
    </button>
  </div>
</div>

<style>
  .edit-heading {
    color: var(--accent-medium);
    margin: 0 0 1rem;
    font-size: var(--text-xl);
  }

  .edit-form {
    background: var(--bg-surface);
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    padding: 1.25rem;
    margin-bottom: 1rem;
  }

  .field {
    margin-bottom: 1rem;
  }

  .avatar-row {
    display: flex;
    align-items: center;
    gap: 0.75rem;
  }

  .avatar-edit-preview {
    width: 56px;
    height: 56px;
    border-radius: 50%;
    object-fit: cover;
    flex-shrink: 0;
  }

  .avatar-fallback {
    width: 56px;
    height: 56px;
    border-radius: 50%;
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: var(--text-icon);
    font-weight: 700;
    color: var(--text-on-accent);
    flex-shrink: 0;
    text-transform: uppercase;
  }

  .avatar-actions {
    display: flex;
    gap: 0.5rem;
  }

  .avatar-btn {
    background: var(--bg-elevated);
    color: var(--accent-light);
    border: none;
    border-radius: var(--radius-sm);
    padding: 0.3rem 0.75rem;
    font-size: var(--text-base);
    cursor: pointer;
  }

  .avatar-btn:hover:not(:disabled) {
    background: var(--bg-elevated-hover);
  }

  .avatar-btn.remove {
    color: var(--color-error-light);
  }

  .avatar-btn.remove:hover {
    background: var(--color-error-light-bg);
  }

  .edit-form input:not([type="checkbox"]):not([type="radio"]),
  .edit-form textarea {
    width: 100%;
    background: var(--bg-deep);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 0.6rem 0.75rem;
    color: var(--text-primary);
    font-size: var(--text-base);
    outline: none;
    transition: border-color var(--transition-normal);
    resize: vertical;
  }

  .edit-form input:not([type="checkbox"]):not([type="radio"]):focus,
  .edit-form textarea:focus {
    border-color: var(--accent-medium);
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
    background: var(--bg-deep);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    cursor: pointer;
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

  .edit-error {
    color: var(--color-error-light);
    font-size: var(--text-sm);
    margin: 0 0 0.5rem;
  }

  .edit-actions {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.25rem;
  }

  .cancel-btn {
    flex: 1;
    padding: 0.6rem;
    font-size: var(--text-base);
  }

  .save-btn {
    flex: 1;
    padding: 0.6rem;
    font-size: var(--text-base);
  }
</style>
