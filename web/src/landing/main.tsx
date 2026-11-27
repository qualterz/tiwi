// @ts-expect-error @carbon/icons ships untyped ESM files
import arrowUpRightIcon from "@carbon/icons/es/arrow--up-right/16.js";
// @ts-expect-error @carbon/icons ships untyped ESM files
import checkmarkIcon from "@carbon/icons/es/checkmark/16.js";
// @ts-expect-error @carbon/icons ships untyped ESM files
import copyIcon from "@carbon/icons/es/copy/16.js";
// @ts-expect-error @carbon/icons ships untyped ESM files
import downloadIcon from "@carbon/icons/es/download/16.js";
// @ts-expect-error @carbon/icons ships untyped ESM files
import logoGithubIcon from "@carbon/icons/es/logo--github/16.js";
import { createSignal, For, onCleanup, onMount } from "solid-js";
import { render } from "solid-js/web";
import "../base.css";
import { CarbonIcon } from "../carbon-icon.tsx";
import "./landing.css";

type InstallTabId = "windows" | "macos" | "linux" | "npm";

const INSTALL_TABS: Array<{ id: InstallTabId; label: string }> = [
  { id: "windows", label: "Windows" },
  { id: "macos", label: "macOS" },
  { id: "linux", label: "Linux" },
  { id: "npm", label: "npm" },
];

const MSI_DOWNLOAD_URL =
  "https://github.com/qualterz/tiwi/releases/latest/download/tiwi-obs-windows-x64.msi";
const RELEASES_URL = "https://github.com/qualterz/tiwi/releases/latest";
const REPO_URL = "https://github.com/qualterz/tiwi";
const OBS_URL = "https://obsproject.com/";
const PS_INSTALL_CMD = "irm https://qualterz.github.io/tiwi/install.ps1 | iex";
const SH_INSTALL_CMD =
  "curl -fsSL https://qualterz.github.io/tiwi/install.sh | sh";
const NPX_CMD = "npx @qualterz/tiwi";

function detectInitialTab(): InstallTabId {
  const uaData = (
    navigator as Navigator & { userAgentData?: { platform?: string } }
  ).userAgentData;
  const platform = (uaData?.platform ?? "").toLowerCase();
  if (platform.includes("win")) return "windows";
  if (platform.includes("mac")) return "macos";
  if (platform.includes("linux")) return "linux";

  const ua = navigator.userAgent.toLowerCase();
  const navPlatform = (navigator.platform ?? "").toLowerCase();
  if (ua.includes("android") || ua.includes("iphone") || ua.includes("ipad")) {
    return "windows";
  }
  if (navPlatform.includes("mac") || ua.includes("mac")) return "macos";
  if (navPlatform.includes("linux") || ua.includes("linux")) return "linux";
  return "windows";
}

function setupCopyCommands(): () => void {
  const timers = new WeakMap<
    HTMLButtonElement,
    ReturnType<typeof setTimeout>
  >();
  let lastMessage = "";

  const report = (button: HTMLButtonElement, ok: boolean) => {
    const tip = button.nextElementSibling;
    clearTimeout(timers.get(button));
    button.removeAttribute("data-copied");
    button.removeAttribute("data-failed");

    let message = ok ? "Copied" : "Copy failed";
    if (message === lastMessage) message += "\u00A0";
    lastMessage = message;

    if (tip) tip.textContent = "";
    void button.offsetWidth;
    button.setAttribute(ok ? "data-copied" : "data-failed", "");
    requestAnimationFrame(() => {
      if (tip) tip.textContent = message;
    });

    timers.set(
      button,
      setTimeout(() => {
        button.removeAttribute("data-copied");
        button.removeAttribute("data-failed");
        if (tip) tip.textContent = "";
      }, 2000),
    );
  };

  const copyFromTarget = async (
    button: HTMLButtonElement,
    target: HTMLElement,
  ) => {
    const text =
      target instanceof HTMLInputElement ||
      target instanceof HTMLTextAreaElement
        ? target.value
        : target.innerText.trim();
    if (!text) return;
    try {
      await navigator.clipboard.writeText(text);
      report(button, true);
    } catch {
      report(button, false);
    }
  };

  if ("CommandEvent" in window) {
    const onCommand = (event: Event) => {
      const cmdEvent = event as Event & {
        command?: string;
        source?: Element | null;
      };
      if (!cmdEvent.command?.startsWith("--copy")) return;
      const button = cmdEvent.source;
      const target = cmdEvent.target;
      if (
        !(button instanceof HTMLButtonElement) ||
        !(target instanceof HTMLElement)
      ) {
        return;
      }
      void copyFromTarget(button, target);
    };
    document.addEventListener("command", onCommand, true);
    return () => document.removeEventListener("command", onCommand, true);
  }

  const onFallbackClick = (event: MouseEvent) => {
    if (event.defaultPrevented) return;
    if (!(event.target instanceof Element)) return;
    const button = event.target.closest('button[command^="--copy"]');
    if (!(button instanceof HTMLButtonElement)) return;
    const targetId = button.getAttribute("commandfor");
    if (!targetId) return;
    const target = document.getElementById(targetId);
    if (target) void copyFromTarget(button, target);
  };
  document.addEventListener("click", onFallbackClick);
  return () => document.removeEventListener("click", onFallbackClick);
}

function CommandBlock(props: { id: string; command: string; label: string }) {
  return (
    <div class="flex items-stretch border border-white/15 bg-zinc-950">
      <pre class="min-w-0 flex-1 overflow-x-auto px-3.5 py-2.5 font-mono text-xs text-white/90 sm:text-sm">
        <code id={props.id}>{props.command}</code>
      </pre>
      <span class="relative inline-flex shrink-0 items-stretch border-l border-white/15">
        <button
          type="button"
          {...({ command: "--copy", commandfor: props.id } as Record<
            string,
            string
          >)}
          aria-label={`Copy ${props.label}`}
          class="copy-btn inline-flex min-h-10 min-w-10 cursor-pointer items-center justify-center bg-transparent text-white/70 transition-colors hover:bg-[var(--control-muted)] hover:text-white data-[copied]:bg-[var(--control-muted)] data-[copied]:text-white data-[failed]:text-red-500"
        >
          <CarbonIcon icon={copyIcon} class="copy-icon-copy h-4 w-4" />
          <CarbonIcon icon={checkmarkIcon} class="copy-icon-check h-4 w-4" />
        </button>
        <span
          role="status"
          class="copy-tip absolute right-0 bottom-[calc(100%+8px)] whitespace-nowrap"
        />
      </span>
    </div>
  );
}

export default function Landing() {
  const [activeTab, setActiveTab] =
    createSignal<InstallTabId>(detectInitialTab());
  const tabRefs = new Map<InstallTabId, HTMLButtonElement>();

  onMount(() => {
    const cleanupCopy = setupCopyCommands();
    onCleanup(cleanupCopy);
  });

  const selectTab = (tabId: InstallTabId, moveFocus = false) => {
    setActiveTab(tabId);
    const tab = tabRefs.get(tabId);
    if (tab) {
      tab.scrollIntoView({ block: "nearest", inline: "nearest" });
      if (moveFocus) tab.focus();
    }
  };

  const onTabKeyDown = (e: KeyboardEvent) => {
    const idx = INSTALL_TABS.findIndex((t) => t.id === activeTab());
    if (idx < 0) return;

    let nextIdx: number | null = null;
    if (e.key === "ArrowRight") nextIdx = (idx + 1) % INSTALL_TABS.length;
    else if (e.key === "ArrowLeft")
      nextIdx = (idx - 1 + INSTALL_TABS.length) % INSTALL_TABS.length;
    else if (e.key === "Home") nextIdx = 0;
    else if (e.key === "End") nextIdx = INSTALL_TABS.length - 1;

    if (nextIdx !== null) {
      e.preventDefault();
      const target = INSTALL_TABS[nextIdx];
      if (target) selectTab(target.id, true);
    }
  };

  return (
    <div class="min-h-screen bg-zinc-950 px-4 py-12 sm:px-6 sm:py-16">
      <main class="mx-auto w-full max-w-2xl space-y-12">
        <header class="@container space-y-4">
          <div class="flex items-center justify-between gap-4">
            <div class="inline-flex flex-col items-start gap-[3px] font-sans text-[11px] font-medium uppercase tracking-[0.08em] text-white">
              <span class="status-label leading-none">Tiwi</span>
              <span aria-hidden="true" class="h-[2px] w-full bg-red-500" />
            </div>
            <a
              href={REPO_URL}
              target="_blank"
              rel="noopener noreferrer"
              class="inline-flex items-center gap-1.5 font-sans text-xs font-medium uppercase tracking-[0.08em] text-white/60 transition-colors hover:text-white"
            >
              <CarbonIcon icon={logoGithubIcon} class="h-4 w-4" />
              <span>GitHub</span>
            </a>
          </div>
          <h1 class="landing-heading text-[clamp(1.375rem,3.85cqi,1.6875rem)] font-semibold tracking-tight text-white">
            Zero-infrastructure peer-to-peer live streaming.
          </h1>
          <p class="landing-copy text-base leading-relaxed text-white/70">
            Stream straight from{" "}
            <a
              href={OBS_URL}
              target="_blank"
              rel="noopener noreferrer"
              class="text-white underline decoration-white/30 underline-offset-4 transition-colors hover:decoration-white"
            >
              OBS Studio 33+
            </a>{" "}
            to browsers over WebRTC with no servers to host.
          </p>
        </header>

        <section aria-label="Install Tiwi">
          <div class="flex flex-wrap items-center justify-between gap-x-4 border-b border-white/15">
            <div
              role="tablist"
              aria-label="Installation method"
              {...({ focusgroup: "tablist nomemory" } as Record<
                string,
                string
              >)}
              onKeyDown={onTabKeyDown}
              class="flex overflow-x-auto"
            >
              <For each={INSTALL_TABS}>
                {(tab) => {
                  const selected = () => activeTab() === tab.id;
                  return (
                    <button
                      ref={(el) => tabRefs.set(tab.id, el)}
                      type="button"
                      role="tab"
                      id={`tab-${tab.id}`}
                      aria-selected={selected()}
                      tabIndex={selected() ? 0 : -1}
                      {...(selected() ? { focusgroupstart: "" } : {})}
                      onClick={() => selectTab(tab.id)}
                      class={`os-tab shrink-0 cursor-pointer px-4 py-2.5 text-xs font-medium uppercase tracking-[0.08em] transition-colors ${
                        selected()
                          ? "text-white"
                          : "text-white/50 hover:text-white"
                      }`}
                    >
                      {tab.label}
                    </button>
                  );
                }}
              </For>
            </div>
            <a
              href={RELEASES_URL}
              target="_blank"
              rel="noopener noreferrer"
              class="inline-flex shrink-0 items-center gap-1 py-2.5 text-xs font-medium uppercase tracking-[0.08em] text-white/60 transition-colors hover:text-white"
            >
              <span>Releases</span>
              <CarbonIcon icon={arrowUpRightIcon} class="h-3.5 w-3.5" />
            </a>
          </div>

          <div
            role="tabpanel"
            aria-labelledby="tab-windows"
            hidden={activeTab() !== "windows"}
          >
            <div class="space-y-5 border border-t-0 border-white/15 bg-zinc-900 p-5 sm:p-6">
              <div>
                <a
                  href={MSI_DOWNLOAD_URL}
                  class="inline-flex items-center gap-2.5 bg-white px-4 py-2.5 text-sm font-medium text-zinc-950 transition-colors hover:bg-white/90"
                >
                  <CarbonIcon icon={downloadIcon} class="h-4 w-4" />
                  <span>Download Installer (.msi)</span>
                </a>
              </div>

              <div class="space-y-2 border-t border-white/15 pt-4">
                <p class="font-sans text-[11px] font-medium uppercase tracking-[0.08em] text-white/60">
                  PowerShell
                </p>
                <CommandBlock
                  id="cmd-win-ps"
                  command={PS_INSTALL_CMD}
                  label="Windows PowerShell"
                />
              </div>
            </div>
          </div>

          <div
            role="tabpanel"
            aria-labelledby="tab-macos"
            hidden={activeTab() !== "macos"}
          >
            <div class="space-y-4 border border-t-0 border-white/15 bg-zinc-900 p-5 sm:p-6">
              <div class="space-y-2">
                <p class="font-sans text-[11px] font-medium uppercase tracking-[0.08em] text-white/60">
                  Terminal · Universal
                </p>
                <CommandBlock
                  id="cmd-macos-sh"
                  command={SH_INSTALL_CMD}
                  label="macOS terminal"
                />
              </div>
            </div>
          </div>

          <div
            role="tabpanel"
            aria-labelledby="tab-linux"
            hidden={activeTab() !== "linux"}
          >
            <div class="space-y-4 border border-t-0 border-white/15 bg-zinc-900 p-5 sm:p-6">
              <div class="space-y-2">
                <p class="font-sans text-[11px] font-medium uppercase tracking-[0.08em] text-white/60">
                  Terminal · x86_64
                </p>
                <CommandBlock
                  id="cmd-linux-sh"
                  command={SH_INSTALL_CMD}
                  label="Linux terminal"
                />
              </div>
              <p class="text-xs text-white/50">
                Detects native and Flatpak OBS automatically.
              </p>
            </div>
          </div>

          <div
            role="tabpanel"
            aria-labelledby="tab-npm"
            hidden={activeTab() !== "npm"}
          >
            <div class="space-y-4 border border-t-0 border-white/15 bg-zinc-900 p-5 sm:p-6">
              <div class="space-y-2">
                <p class="font-sans text-[11px] font-medium uppercase tracking-[0.08em] text-white/60">
                  Standalone WHIP daemon · Node.js 18+
                </p>
                <CommandBlock
                  id="cmd-npm"
                  command={NPX_CMD}
                  label="npm command"
                />
              </div>
              <p class="text-xs text-white/50">
                Works with OBS (Settings → Stream → WHIP →{" "}
                <code class="font-mono text-white/80">
                  http://127.0.0.1:2000/whip
                </code>
                ) or any WHIP broadcaster.
              </p>
            </div>
          </div>
        </section>

        <section
          aria-label="How it works"
          class="space-y-4 border-t border-white/10 pt-8"
        >
          <h2 class="font-sans text-[11px] font-medium uppercase tracking-[0.08em] text-white/45">
            Quick start
          </h2>
          <ol class="grid gap-6 sm:grid-cols-3">
            <li>
              <span class="block font-mono text-[11px] text-white/45">01</span>
              <p class="mt-2 text-sm font-medium text-white">
                Install &amp; open OBS
              </p>
              <p class="landing-copy mt-1 text-xs leading-relaxed text-white/60">
                The Tiwi dock appears automatically on your first launch.
              </p>
            </li>
            <li>
              <span class="block font-mono text-[11px] text-white/45">02</span>
              <p class="mt-2 text-sm font-medium text-white">Accept defaults</p>
              <p class="landing-copy mt-1 text-xs leading-relaxed text-white/60">
                Use Tiwi&apos;s recommended low-latency WebRTC preset or keep
                your OBS settings.
              </p>
            </li>
            <li>
              <span class="block font-mono text-[11px] text-white/45">03</span>
              <p class="mt-2 text-sm font-medium text-white">Share the link</p>
              <p class="landing-copy mt-1 text-xs leading-relaxed text-white/60">
                Click Start Streaming and Copy Link. Viewers watch right in the
                browser.
              </p>
            </li>
          </ol>
        </section>
      </main>
    </div>
  );
}

render(() => <Landing />, document.getElementById("root")!);
