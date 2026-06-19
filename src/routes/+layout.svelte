<script lang="ts">
  import "../app.css";
  import { page } from "$app/state";
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { getCurrentWebview } from "@tauri-apps/api/webview";
  import { onMount } from "svelte";
  import {
    isPermissionGranted,
    requestPermission,
    sendNotification,
  } from "@tauri-apps/plugin-notification";
  import { onOpenUrl } from "@tauri-apps/plugin-deep-link";
  import { goto } from "$app/navigation";
  import type {
    NodeStatus,
    Post,
    StoredMessage,
    CallEvent,
    CallState,
    StageAnnouncement,
  } from "$lib/types";
  import Sidebar from "$lib/Sidebar.svelte";
  import RightSidebar from "$lib/RightSidebar.svelte";
  import BottomNav from "$lib/BottomNav.svelte";
  import MobileHeader from "$lib/MobileHeader.svelte";
  import CallOverlay from "$lib/CallOverlay.svelte";
  import { applyAccent, DEFAULT_ACCENT } from "$lib/accent";

  const ZOOM_KEY = "app-zoom-level";
  const ZOOM_STEP = 0.2;
  const ZOOM_MIN = 0.2;
  const ZOOM_MAX = 10.0;

  let { children } = $props();
  let status = $state<NodeStatus | null>(null);
  let zoomLevel = $state(1.0);
  let unreadDmCount = $state(0);
  let unreadNotificationCount = $state(0);
  let nodeId = $state("");
  let pubkey = $state("");
  let activeCallId = $state<string | null>(null);
  let activeCallPeer = $state("");
  let activeCallState = $state<CallState | null>(null);
  let liveStages = $state<Map<string, StageAnnouncement>>(new Map());

  async function applyZoom(level: number) {
    zoomLevel = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, level));
    localStorage.setItem(ZOOM_KEY, String(zoomLevel));
    await getCurrentWebview().setZoom(zoomLevel);
  }

  function handleZoomKeys(e: KeyboardEvent) {
    const mod = e.ctrlKey || e.metaKey;
    if (!mod) return;
    if (e.key === "=" || e.key === "+") {
      e.preventDefault();
      applyZoom(zoomLevel + ZOOM_STEP);
    } else if (e.key === "-") {
      e.preventDefault();
      applyZoom(zoomLevel - ZOOM_STEP);
    } else if (e.key === "0") {
      e.preventDefault();
      applyZoom(1.0);
    }
  }

  function handleDeepLink(url: string) {
    try {
      const parsed = new URL(url);
      if (parsed.protocol !== "proscenium:") return;
      if (parsed.hostname === "profile" || parsed.hostname === "user") {
        const id = parsed.pathname.slice(1);
        if (id) {
          const transport = parsed.searchParams.get("transport");
          const query = transport ? `?transport=${transport}` : "";
          goto(`/profile/${id}${query}`);
        }
      }
    } catch {
      // malformed URL, ignore
    }
  }

  async function pollStatus() {
    try {
      status = await invoke("get_node_status");
    } catch {
      // Node not ready yet
    }
  }

  async function pollUnread() {
    try {
      unreadDmCount = await invoke("get_unread_dm_count");
    } catch {
      // Node not ready yet
    }
  }

  async function pollUnreadNotifications() {
    try {
      unreadNotificationCount = await invoke("get_unread_notification_count");
    } catch {
      // Node not ready yet
    }
  }

  onMount(() => {
    const saved = localStorage.getItem(ZOOM_KEY);
    if (saved) {
      const parsed = parseFloat(saved);
      if (Number.isFinite(parsed)) {
        applyZoom(parsed);
      }
    }

    window.addEventListener("keydown", handleZoomKeys);
    invoke<string>("get_node_id")
      .then((id) => (nodeId = id))
      .catch(() => {});
    invoke<string>("get_pubkey")
      .then((id) => (pubkey = id))
      .catch(() => {});
    invoke<string | null>("get_accent_color")
      .then((name) => applyAccent(name ?? DEFAULT_ACCENT))
      .catch(() => applyAccent(DEFAULT_ACCENT));
    pollStatus();
    pollUnread();
    pollUnreadNotifications();
    const statusInterval = setInterval(pollStatus, 10000);
    const unreadInterval = setInterval(pollUnread, 10000);
    const notificationInterval = setInterval(pollUnreadNotifications, 10000);
    const unlisteners: Promise<UnlistenFn>[] = [];

    async function setupNotifications() {
      let permitted = await isPermissionGranted();
      if (!permitted) {
        const result = await requestPermission();
        permitted = result === "granted";
      }
      unlisteners.push(
        listen<{ from: string; message: StoredMessage }>(
          "dm-received",
          async (event) => {
            pollUnread();
            const senderPubkey = event.payload.from;
            const isViewingConversation =
              page.url.pathname === `/messages/${senderPubkey}`;
            if (!isViewingConversation && permitted) {
              let title = senderPubkey.slice(0, 8);
              try {
                const profile = await invoke<{ display_name: string } | null>(
                  "get_remote_profile",
                  { pubkey: senderPubkey },
                );
                if (profile?.display_name) {
                  title = profile.display_name;
                }
              } catch {
                // keep short pubkey as title
              }
              sendNotification({
                title,
                body: event.payload.message.content || "Sent a message",
              });
            }
          },
        ),
      );
      unlisteners.push(
        listen<Post>("mentioned-in-post", async (event) => {
          pollUnreadNotifications();
          const isViewingActivity = page.url.pathname === "/activity";
          if (!isViewingActivity && permitted) {
            const post = event.payload;
            let title = post.author.slice(0, 8);
            try {
              const profile = await invoke<{ display_name: string } | null>(
                "get_remote_profile",
                { pubkey: post.author },
              );
              if (profile?.display_name) {
                title = profile.display_name;
              }
            } catch {
              // keep short pubkey as title
            }
            sendNotification({
              title: `${title} mentioned you`,
              body: post.content.slice(0, 100) || "Mentioned you in a post",
            });
          }
        }),
      );
      unlisteners.push(
        listen("notification-received", () => {
          pollUnreadNotifications();
        }),
      );
      unlisteners.push(
        listen<CallEvent>("call-state", (event) => {
          const { call_id, peer_pubkey, state } = event.payload;
          if (state === "ended" || state === "failed") {
            // Show briefly then dismiss
            activeCallId = call_id;
            activeCallPeer = peer_pubkey;
            activeCallState = state;
            setTimeout(() => {
              if (activeCallId === call_id) {
                activeCallId = null;
                activeCallState = null;
              }
            }, 2000);
          } else {
            activeCallId = call_id;
            activeCallPeer = peer_pubkey;
            activeCallState = state;
          }
        }),
      );
    }

    setupNotifications();

    // Stage announcements (drive the right sidebar live-stages panel)
    unlisteners.push(
      listen<StageAnnouncement>("stage-announced", (event) => {
        const ann = event.payload;
        liveStages = new Map(liveStages).set(ann.stage_id, ann);
      }),
    );
    unlisteners.push(
      listen<string>("stage-ended-remote", (event) => {
        const next = new Map(liveStages);
        next.delete(event.payload);
        liveStages = next;
      }),
    );

    // Deep link handling
    unlisteners.push(
      onOpenUrl((urls) => {
        for (const url of urls) {
          handleDeepLink(url);
        }
      }),
    );
    unlisteners.push(
      listen<string[]>("deep-link-received", (event) => {
        for (const url of event.payload) {
          handleDeepLink(url);
        }
      }),
    );

    return () => {
      window.removeEventListener("keydown", handleZoomKeys);
      clearInterval(statusInterval);
      clearInterval(unreadInterval);
      clearInterval(notificationInterval);
      unlisteners.forEach((p) => p.then((fn) => fn()));
    };
  });
</script>

<div class="app-shell">
  <Sidebar
    {pubkey}
    {status}
    {unreadDmCount}
    {unreadNotificationCount}
    currentPath={page.url.pathname}
  />

  <MobileHeader {status} />

  <div class="main-column">
    {#if status && !status.has_relay}
      <div class="relay-banner">
        <span class="relay-banner-dot"></span>
        <span>Relay disconnected -- messages and sync may not work</span>
      </div>
    {/if}
    <main>
      {@render children()}
    </main>
  </div>

  <RightSidebar {liveStages} />

  <BottomNav
    {pubkey}
    {unreadDmCount}
    {unreadNotificationCount}
    currentPath={page.url.pathname}
  />
</div>

{#if activeCallId && activeCallState}
  <CallOverlay
    callId={activeCallId}
    peerPubkey={activeCallPeer}
    callState={activeCallState}
    selfId={pubkey}
  />
{/if}

<style>
  .app-shell {
    min-height: 100vh;
  }

  .main-column {
    min-height: 100vh;
    display: flex;
    flex-direction: column;
    position: relative;
  }

  main {
    max-width: var(--content-max-width);
    width: 100%;
    margin: 0 auto;
    padding: var(--space-lg) var(--space-xl);
    padding-bottom: calc(
      var(--bottom-nav-height) + env(safe-area-inset-bottom) + var(--space-lg)
    );
    flex: 1;
  }

  .relay-banner {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem 1rem;
    background: var(--danger-bg);
    border-bottom: 1px solid var(--danger-border);
    color: var(--danger-text);
    font-size: var(--text-base);
    font-weight: 500;
  }

  .relay-banner-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--color-error);
    flex-shrink: 0;
    animation: pulse-dot 2s infinite;
  }

  @keyframes pulse-dot {
    0%,
    100% {
      opacity: 1;
    }
    50% {
      opacity: 0.4;
    }
  }

  @media (min-width: 768px) {
    .app-shell {
      padding-left: var(--sidebar-width);
    }

    main {
      padding-bottom: var(--space-lg);
    }
  }

  @media (min-width: 1150px) {
    .app-shell {
      padding-right: var(--right-sidebar-width);
    }
  }
</style>
