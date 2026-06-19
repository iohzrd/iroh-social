<script lang="ts">
  import { onMount } from "svelte";
  import { hapticNotification } from "$lib/haptics";
  import type { LinkQrPayload } from "$lib/types";

  interface Props {
    onscanned: (payload: LinkQrPayload) => void;
    onclose: () => void;
  }

  let { onscanned, onclose }: Props = $props();

  let error = $state<string | null>(null);
  let scanning = $state(true);
  let manualInput = $state("");
  let showManual = $state(false);
  let cancelFn: (() => Promise<void>) | null = null;

  function parsePayload(text: string): LinkQrPayload | null {
    try {
      const parsed = JSON.parse(text);
      if (
        typeof parsed.node_id === "string" &&
        typeof parsed.secret === "string"
      ) {
        return {
          node_id: parsed.node_id,
          secret: parsed.secret,
          relay_url: parsed.relay_url ?? null,
        };
      }
      return null;
    } catch {
      return null;
    }
  }

  function handleScanResult(text: string) {
    const payload = parsePayload(text);
    if (payload) {
      hapticNotification("success");
      onscanned(payload);
    } else {
      error = "Not a valid device link QR code";
    }
  }

  function submitManual() {
    const payload = parsePayload(manualInput.trim());
    if (payload) {
      onscanned(payload);
    } else {
      error = "Invalid payload. Paste the JSON from the existing device.";
    }
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === "Escape") onclose();
  }

  async function stopScanning() {
    if (cancelFn) {
      try {
        await cancelFn();
      } catch {
        // ignore cancel errors
      }
      cancelFn = null;
    }
    document.documentElement.style.background = "";
    document.body.style.background = "";
  }

  onMount(() => {
    (async () => {
      try {
        const { scan, cancel, Format, checkPermissions, requestPermissions } =
          await import("@tauri-apps/plugin-barcode-scanner");

        cancelFn = cancel;

        let perms = await checkPermissions();
        if (perms !== "granted") {
          perms = await requestPermissions();
        }
        if (perms !== "granted") {
          // No camera -- fall back to manual input
          scanning = false;
          showManual = true;
          return;
        }

        document.documentElement.style.background = "transparent";
        document.body.style.background = "transparent";

        const result = await scan({
          formats: [Format.QRCode],
          windowed: true,
        });
        scanning = false;
        await stopScanning();
        handleScanResult(result.content);
      } catch {
        // Scanner not available (desktop) -- fall back to manual input
        scanning = false;
        showManual = true;
        await stopScanning();
      }
    })();

    return () => {
      stopScanning();
    };
  });
</script>

<svelte:window onkeydown={handleKeydown} />

{#if scanning}
  <div class="scanner-overlay">
    <div class="scanner-top"></div>
    <div class="scanner-middle">
      <div class="scanner-side"></div>
      <div class="scanner-viewfinder"></div>
      <div class="scanner-side"></div>
    </div>
    <div class="scanner-bottom">
      <p class="scanner-hint">Scan device link QR code</p>
      <button class="scanner-close" onclick={onclose}>Cancel</button>
    </div>
  </div>
{/if}

{#if showManual || error}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <div class="modal-overlay" onclick={onclose} role="presentation">
    <!-- svelte-ignore a11y_interactive_supports_focus -->
    <div
      class="modal link-scanner-modal"
      onclick={(e) => e.stopPropagation()}
      role="dialog"
      aria-label="Link with device"
    >
      {#if error}
        <p class="scanner-error">{error}</p>
      {/if}
      <p class="scanner-label">
        Paste the link payload from your other device:
      </p>
      <textarea
        class="textarea-base manual-input"
        bind:value={manualInput}
        placeholder="node_id, secret, relay_url (JSON)"
        rows="4"></textarea>
      <div class="modal-actions">
        <button class="modal-cancel" onclick={onclose}>Cancel</button>
        <button
          class="modal-confirm"
          onclick={submitManual}
          disabled={!manualInput.trim()}
        >
          Link
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .scanner-overlay {
    position: fixed;
    inset: 0;
    display: flex;
    flex-direction: column;
    z-index: var(--z-scanner);
  }

  .scanner-top {
    flex: 1;
    background: var(--overlay-medium);
  }

  .scanner-middle {
    display: flex;
  }

  .scanner-side {
    flex: 1;
    background: var(--overlay-medium);
  }

  .scanner-viewfinder {
    width: 250px;
    height: 250px;
    border: 3px solid var(--accent-medium);
    border-radius: var(--radius-2xl);
    flex-shrink: 0;
  }

  .scanner-bottom {
    flex: 1;
    background: var(--overlay-medium);
    display: flex;
    flex-direction: column;
    align-items: center;
    padding-top: 1rem;
  }

  .scanner-hint {
    color: var(--text-on-accent);
    font-size: var(--text-base);
    margin: 0;
    text-shadow: 0 1px 4px rgba(0, 0, 0, 0.8);
  }

  .scanner-close {
    margin-top: 1.5rem;
    background: var(--overlay-light);
    color: var(--text-on-accent);
    border: 1px solid var(--overlay-white-faint);
    border-radius: var(--radius-lg);
    padding: 0.6rem 2rem;
    font-size: var(--text-icon);
    cursor: pointer;
    transition: background var(--transition-fast);
  }

  .scanner-close:hover {
    background: var(--overlay-medium);
  }

  .link-scanner-modal {
    max-width: 380px;
  }

  .scanner-label {
    margin: 0 0 0.75rem;
    color: var(--text-secondary);
    font-size: var(--text-sm);
  }

  .scanner-error {
    margin: 0 0 0.75rem;
    color: var(--color-error, #ef4444);
    font-size: var(--text-sm);
  }

  .manual-input {
    width: 100%;
    margin-bottom: 0.75rem;
    font-size: var(--text-xs);
    font-family: monospace;
  }
</style>
