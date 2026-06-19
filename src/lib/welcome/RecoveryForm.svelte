<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";

  let {
    onback,
    onprofile,
  }: {
    onback: () => void;
    onprofile: () => void;
  } = $props();

  let recoveryPhrase = $state("");
  let recovering = $state(false);
  let recoveryError = $state("");
  let recoveryComplete = $state(false);

  async function recoverFromPhrase() {
    const trimmed = recoveryPhrase.trim();
    const words = trimmed.split(/\s+/);
    if (words.length !== 24) {
      recoveryError = "Recovery phrase must be exactly 24 words.";
      return;
    }
    recovering = true;
    recoveryError = "";
    try {
      await invoke("recover_from_seed_phrase", { phrase: trimmed });
      recoveryComplete = true;
    } catch (e) {
      recoveryError = `Recovery failed: ${e}`;
    }
    recovering = false;
  }
</script>

<div class="step">
  {#if recoveryComplete}
    <h2>Identity Recovered</h2>
    <p class="desc">
      Your identity has been restored. Set up your profile to get started.
    </p>
    <button class="btn-accent primary-btn" onclick={onprofile}>
      Create Profile
    </button>
  {:else}
    <h2>Recover Your Identity</h2>
    <p class="desc">
      Enter your 24-word recovery phrase to restore your identity on this
      device. This will replace any existing identity.
    </p>

    <label class="field">
      <span class="field-label">Recovery Phrase</span>
      <textarea
        class="input-base recovery-textarea"
        bind:value={recoveryPhrase}
        placeholder="Enter your 24 words separated by spaces"
        rows="4"
        autocapitalize="none"
        autocomplete="off"
        spellcheck="false"></textarea>
    </label>

    {#if recoveryError}
      <p class="error">{recoveryError}</p>
    {/if}

    <div class="actions">
      <button class="secondary-btn" onclick={onback}>Back</button>
      <button
        class="btn-accent primary-btn"
        onclick={recoverFromPhrase}
        disabled={!recoveryPhrase.trim() || recovering}
      >
        {recovering ? "Recovering..." : "Recover Identity"}
      </button>
    </div>
  {/if}
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

  .desc {
    color: var(--text-secondary);
    font-size: var(--text-base);
    line-height: 1.6;
    margin: 0 0 1.5rem;
  }

  .field {
    display: block;
    text-align: left;
  }

  .recovery-textarea {
    font-family: var(--font-mono);
    font-size: var(--text-sm);
    resize: vertical;
    min-height: 80px;
  }

  .error {
    color: var(--color-danger);
    font-size: var(--text-sm);
    margin: 0 0 1rem;
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
</style>
