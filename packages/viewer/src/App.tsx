// @ts-expect-error @carbon/icons ships untyped ESM files
import playFilledIcon from "@carbon/icons/es/play--filled--alt/16.js";
import { createEffect, createSignal, For, onCleanup, onMount } from "solid-js";
import { Dynamic } from "solid-js/web";
import {
  startViewer,
  type ViewerController,
  type ViewerState,
} from "./viewer.ts";

interface CarbonIconDescriptor {
  attrs: Record<string, string | number>;
  content: Array<{ elem: string; attrs: Record<string, string | number> }>;
}

function CarbonIcon(props: { icon: CarbonIconDescriptor; class?: string }) {
  return (
    <svg
      {...props.icon.attrs}
      class={props.class}
      fill="currentColor"
      aria-hidden="true"
    >
      <For each={props.icon.content}>
        {(el) => <Dynamic component={el.elem} {...el.attrs} />}
      </For>
    </svg>
  );
}

export default function App() {
  const [viewerState, setViewerState] = createSignal<ViewerState>("connecting");
  const [loading, setLoading] = createSignal(true);
  const [videoPaused, setVideoPaused] = createSignal(true);
  const paused = () =>
    viewerState() === "paused" || (viewerState() === "live" && videoPaused());
  const [liveBadge, setLiveBadge] = createSignal(false);
  let liveTimer: ReturnType<typeof setTimeout> | undefined;
  let wasLoading = false;

  createEffect(() => {
    const isLivePlaying = viewerState() === "live" && !paused() && !loading();
    const isLoadingState =
      !isLivePlaying && !paused() && viewerState() !== "invalid";

    if (isLoadingState) {
      wasLoading = true;
      clearTimeout(liveTimer);
      setLiveBadge(false);
    } else if (isLivePlaying && wasLoading) {
      wasLoading = false;
      setLiveBadge(true);
      clearTimeout(liveTimer);
      liveTimer = setTimeout(() => setLiveBadge(false), 1500);
      resetIdleTimer();
    } else if (!isLivePlaying) {
      clearTimeout(liveTimer);
      setLiveBadge(false);
    }
  });

  const controlsHidden = () => idle() && !paused();

  const status = () => {
    const s = viewerState();
    if (s === "waiting")
      return {
        label: "Waiting",
        tone: "text-white/70",
        line: "bg-white/25",
        glint: false,
      };
    if (s === "connecting")
      return {
        label: "Connecting",
        tone: "text-white/90",
        line: "bg-white/25 text-white",
        glint: true,
      };
    if (s === "disconnected")
      return {
        label: "Reconnecting",
        tone: "text-white/90",
        line: "bg-white/25 text-white",
        glint: true,
      };
    if (s === "no-internet")
      return {
        label: "No internet",
        tone: "text-white/80",
        line: "bg-white/35",
        glint: false,
      };
    if (s === "timed-out")
      return {
        label: "Timed out",
        tone: "text-white/80",
        line: "bg-white/35",
        glint: false,
      };
    if (s === "unreachable")
      return {
        label: "Unreachable",
        tone: "text-white/80",
        line: "bg-white/35",
        glint: false,
      };
    if (s === "stream-error")
      return {
        label: "Stream error",
        tone: "text-white/80",
        line: "bg-white/35",
        glint: false,
      };
    if (s === "invalid")
      return {
        label: "Invalid link",
        tone: "text-white/80",
        line: "bg-white/35",
        glint: false,
      };
    if (s === "live" && !paused() && loading())
      return {
        label: "Buffering",
        tone: "text-white/90",
        line: "bg-white/25 text-white",
        glint: true,
      };
    if (s === "live" && !paused() && liveBadge())
      return {
        label: "Live",
        tone: "text-white",
        line: "bg-red-500",
        glint: false,
      };
    return null;
  };
  const [lastStatus, setLastStatus] = createSignal(status());
  createEffect(() => {
    const current = status();
    if (current) setLastStatus(current);
  });
  const [muted, setMuted] = createSignal(true);
  const [volume, setVolume] = createSignal(1);
  const [fullscreen, setFullscreen] = createSignal(false);
  const [idle, setIdle] = createSignal(false);
  const [touchControls, setTouchControls] = createSignal(false);
  let containerRef!: HTMLDivElement;
  let videoRef!: HTMLVideoElement;
  let viewer: ViewerController | undefined;
  let clickTimer: ReturnType<typeof setTimeout> | undefined;
  let idleTimer: ReturnType<typeof setTimeout> | undefined;
  let touchTimer: ReturnType<typeof setTimeout> | undefined;

  const resetIdleTimer = () => {
    setIdle(false);
    clearTimeout(idleTimer);
    idleTimer = setTimeout(() => setIdle(true), 5000);
  };

  const onContainerMouseLeave = () => {
    clearTimeout(idleTimer);
    setIdle(false);
  };

  let lastTouch = 0;

  const scheduleTouchHide = () => {
    clearTimeout(touchTimer);
    touchTimer = setTimeout(() => setTouchControls(false), 3000);
  };

  const onContainerTouchStart = (e: TouchEvent) => {
    lastTouch = Date.now();
    if ((touchControls() || paused()) && e.target !== videoRef) {
      setTouchControls(true);
      scheduleTouchHide();
    }
  };

  const onVideoClick = () => {
    if (Date.now() - lastTouch < 500) {
      if (fullscreen()) resetIdleTimer();
      if (touchControls()) {
        clearTimeout(touchTimer);
        setTouchControls(false);
      } else {
        setTouchControls(true);
        scheduleTouchHide();
      }
      return;
    }
    clearTimeout(clickTimer);
    clickTimer = setTimeout(() => {
      togglePlay();
    }, 200);
  };

  const onVideoDblClick = () => {
    clearTimeout(clickTimer);
    toggleFullscreen();
  };

  const togglePlay = () => {
    if (paused()) {
      viewer?.play();
    } else {
      viewer?.pause();
    }
  };

  const toggleMute = () => {
    videoRef.muted = !videoRef.muted;
    setMuted(videoRef.muted);
  };

  const onVolumeInput = (
    e: InputEvent & { currentTarget: HTMLInputElement },
  ) => {
    const val = Number(e.currentTarget.value);
    videoRef.volume = val;
    videoRef.muted = val === 0;
    setVolume(val);
    setMuted(videoRef.muted);
  };

  const toggleFullscreen = () => {
    if (document.fullscreenElement) {
      void document.exitFullscreen();
    } else if (containerRef.requestFullscreen) {
      void containerRef.requestFullscreen();
    } else {
      const webkitVideo = videoRef as HTMLVideoElement & {
        webkitEnterFullscreen?: () => void;
      };
      webkitVideo.webkitEnterFullscreen?.();
    }
  };

  onMount(() => {
    viewer = startViewer(videoRef, setViewerState);
    const onFullscreenChange = () => {
      const isFs = Boolean(document.fullscreenElement);
      setFullscreen(isFs);
      if (isFs) {
        resetIdleTimer();
        void (
          screen.orientation as ScreenOrientation & {
            lock?: (orientation: string) => Promise<void>;
          }
        )
          ?.lock?.("landscape")
          .catch(() => {});
      } else {
        clearTimeout(idleTimer);
        setIdle(false);
        screen.orientation?.unlock?.();
      }
    };
    document.addEventListener("fullscreenchange", onFullscreenChange);
    onCleanup(() => {
      viewer?.destroy();
      clearTimeout(idleTimer);
      clearTimeout(touchTimer);
      clearTimeout(clickTimer);
      clearTimeout(liveTimer);
      document.removeEventListener("fullscreenchange", onFullscreenChange);
    });
  });

  return (
    <main class="flex min-h-screen items-center justify-center p-0 sm:p-4">
      <div
        ref={containerRef}
        onMouseEnter={resetIdleTimer}
        onMouseMove={resetIdleTimer}
        onMouseLeave={onContainerMouseLeave}
        onTouchStart={onContainerTouchStart}
        class={`group relative aspect-video max-h-[90vh] w-full max-w-6xl select-none overflow-hidden bg-zinc-900 ${
          controlsHidden() ? "cursor-none" : ""
        }`}
      >
        <video
          ref={videoRef}
          playsinline
          muted
          onClick={onVideoClick}
          onDblClick={onVideoDblClick}
          onPlay={() => {
            setVideoPaused(false);
            setLoading(true);
          }}
          onPlaying={() => setLoading(false)}
          onWaiting={() => setLoading(true)}
          onPause={() => {
            if (!videoRef.paused) return;
            setVideoPaused(true);
            setLoading(false);
          }}
          onVolumeChange={() => {
            setMuted(videoRef.muted);
            setVolume(videoRef.volume);
          }}
          class={`h-full w-full object-contain ${
            controlsHidden() ? "cursor-none" : "cursor-pointer"
          }`}
        />

        <button
          type="button"
          onClick={togglePlay}
          aria-label={paused() ? "Play" : "Pause"}
          class={`absolute top-1/2 left-1/2 z-10 flex h-16 w-16 -translate-x-1/2 -translate-y-1/2 cursor-pointer items-center justify-center rounded-full bg-black/50 text-white transition-opacity duration-200 ease-out [@media(hover:hover)_and_(pointer:fine)]:hidden ${
            controlsHidden()
              ? "pointer-events-none opacity-0"
              : touchControls() || paused()
                ? "pointer-events-auto opacity-100"
                : "pointer-events-none opacity-0"
          }`}
        >
          {paused() ? (
            <CarbonIcon icon={playFilledIcon} class="h-6 w-6" />
          ) : (
            <svg
              width="24"
              height="24"
              viewBox="0 0 16 16"
              fill="currentColor"
              aria-hidden="true"
            >
              <rect x="3.5" y="3" width="3" height="10" />
              <rect x="9.5" y="3" width="3" height="10" />
            </svg>
          )}
        </button>

        <div
          class={`pointer-events-none absolute inset-x-0 bottom-0 h-24 transition-opacity duration-200 ease-out ${
            status()
              ? "opacity-100"
              : controlsHidden()
                ? "opacity-0"
                : touchControls() || paused()
                  ? "opacity-100"
                  : "opacity-0 group-hover:opacity-100"
          }`}
          style={{
            background:
              "linear-gradient(to top, rgba(0,0,0,0.8) 0%, rgba(0,0,0,0.4) 35%, rgba(0,0,0,0.12) 70%, transparent 100%)",
          }}
        />

        <div
          class={`absolute inset-x-0 bottom-0 flex items-center gap-3 px-3 py-2.5 transition-opacity duration-200 ease-out ${
            status()
              ? "pointer-events-auto opacity-100"
              : controlsHidden()
                ? "pointer-events-none opacity-0"
                : touchControls() || paused()
                  ? "pointer-events-auto opacity-100"
                  : "pointer-events-none opacity-0 group-hover:pointer-events-auto group-hover:opacity-100"
          }`}
        >
          <div class="flex flex-1 items-center">
            <button
              type="button"
              onClick={togglePlay}
              aria-label={paused() ? "Play" : "Pause"}
              class="group/btn relative hidden h-8 w-8 cursor-pointer items-center justify-center overflow-hidden rounded-full text-white [@media(hover:hover)_and_(pointer:fine)]:flex"
            >
              <span
                aria-hidden="true"
                class="pointer-events-none absolute inset-0 scale-75 rounded-full bg-white/20 opacity-0 transition duration-75 ease-out group-hover/btn:scale-100 group-hover/btn:opacity-100"
              />
              {paused() ? (
                <CarbonIcon icon={playFilledIcon} />
              ) : (
                <svg
                  width="16"
                  height="16"
                  viewBox="0 0 16 16"
                  fill="currentColor"
                  aria-hidden="true"
                >
                  <rect x="4" y="3" width="3" height="10" />
                  <rect x="9" y="3" width="3" height="10" />
                </svg>
              )}
            </button>
            <div class="flex items-center">
              <button
                type="button"
                onClick={toggleMute}
                aria-label={muted() ? "Unmute" : "Mute"}
                class="group/btn relative flex h-12 w-12 cursor-pointer items-center justify-center overflow-hidden rounded-full text-white [@media(hover:hover)_and_(pointer:fine)]:h-8 [@media(hover:hover)_and_(pointer:fine)]:w-8"
              >
                <span
                  aria-hidden="true"
                  class="pointer-events-none absolute inset-0 scale-75 rounded-full bg-white/20 opacity-0 transition duration-75 ease-out group-hover/btn:scale-100 group-hover/btn:opacity-100"
                />
                <svg
                  width="16"
                  height="16"
                  viewBox="0 0 16 16"
                  fill="none"
                  aria-hidden="true"
                >
                  <g transform="translate(-0.55 0)">
                    <path
                      d="M2 6.2c0-.44.36-.8.8-.8h2.4L8.6 2.7a.5.5 0 0 1 .8.4v9.8a.5.5 0 0 1-.8.4L5.2 10.6H2.8a.8.8 0 0 1-.8-.8V6.2z"
                      fill="currentColor"
                    />
                    {muted() || volume() === 0 ? (
                      <path
                        d="M11.5 6.2l3 3.6m0-3.6l-3 3.6"
                        stroke="currentColor"
                        stroke-width="1.5"
                        stroke-linecap="round"
                      />
                    ) : (
                      <>
                        <path
                          d="M11.2 5.7a3.2 3.2 0 0 1 0 4.6"
                          stroke="currentColor"
                          stroke-width="1.5"
                          stroke-linecap="round"
                        />
                        {volume() > 0.5 && (
                          <path
                            d="M13.2 3.8a6 6 0 0 1 0 8.4"
                            stroke="currentColor"
                            stroke-width="1.5"
                            stroke-linecap="round"
                          />
                        )}
                      </>
                    )}
                  </g>
                </svg>
              </button>
              <div class="relative ml-2 hidden h-8 items-center [@media(hover:hover)_and_(pointer:fine)]:flex">
                <svg
                  width="93"
                  height="10"
                  viewBox="0 0 93 10"
                  shape-rendering="geometricPrecision"
                  class="pointer-events-none block"
                  aria-hidden="true"
                >
                  <For each={Array.from({ length: 16 }, (_, i) => i)}>
                    {(i) => (
                      <rect
                        x={i * 6}
                        y="0"
                        width="3"
                        height="10"
                        fill={
                          i / 15 <= (muted() ? 0 : volume()) &&
                          !(muted() || volume() === 0)
                            ? "rgba(255,255,255,1)"
                            : "rgba(255,255,255,0.35)"
                        }
                      />
                    )}
                  </For>
                </svg>
                <input
                  type="range"
                  min="0"
                  max="1"
                  step="0.01"
                  value={muted() ? 0 : volume()}
                  onInput={onVolumeInput}
                  aria-label="Volume"
                  class="absolute inset-y-0 left-0 z-10 h-8 w-[calc(100%+6px)] cursor-pointer appearance-none bg-transparent [&::-moz-range-thumb]:h-5 [&::-moz-range-thumb]:w-1.5 [&::-moz-range-thumb]:rounded-none [&::-moz-range-thumb]:border-0 [&::-moz-range-thumb]:bg-white [&::-moz-range-track]:h-1 [&::-moz-range-track]:bg-transparent [&::-webkit-slider-runnable-track]:h-1 [&::-webkit-slider-runnable-track]:bg-transparent [&::-webkit-slider-thumb]:-mt-2 [&::-webkit-slider-thumb]:h-5 [&::-webkit-slider-thumb]:w-1.5 [&::-webkit-slider-thumb]:appearance-none [&::-webkit-slider-thumb]:rounded-none [&::-webkit-slider-thumb]:bg-white"
                />
              </div>
            </div>
          </div>
          <div
            role="status"
            aria-live="polite"
            aria-hidden={!status() || undefined}
            data-active={status() ? "" : undefined}
            class={`status-badge pointer-events-none flex shrink-0 flex-col items-center justify-center gap-[3px] whitespace-nowrap font-sans text-[11px] font-medium uppercase tracking-[0.08em] transition-opacity duration-200 ease-out ${
              lastStatus()?.tone ?? "text-white/80"
            } ${status() ? "opacity-100" : "opacity-0"}`}
          >
            <span class="status-label leading-none">
              {lastStatus()?.label ?? ""}
            </span>
            <span
              aria-hidden="true"
              data-glint={lastStatus()?.glint ? "" : undefined}
              class={`status-line relative h-[2px] w-full overflow-hidden ${lastStatus()?.line ?? ""}`}
            />
          </div>
          <div class="flex flex-1 items-center justify-end">
            <button
              type="button"
              onClick={toggleFullscreen}
              aria-label={fullscreen() ? "Exit fullscreen" : "Fullscreen"}
              class="group/btn relative flex h-12 w-12 cursor-pointer items-center justify-center overflow-hidden rounded-full text-white [@media(hover:hover)_and_(pointer:fine)]:h-8 [@media(hover:hover)_and_(pointer:fine)]:w-8"
            >
              <span
                aria-hidden="true"
                class="pointer-events-none absolute inset-0 scale-75 rounded-full bg-white/20 opacity-0 transition duration-75 ease-out group-hover/btn:scale-100 group-hover/btn:opacity-100"
              />
              <svg
                width="16"
                height="16"
                viewBox="0 0 16 16"
                fill="none"
                stroke="currentColor"
                stroke-width="1.5"
                stroke-linecap="square"
                aria-hidden="true"
              >
                {fullscreen() ? (
                  <path d="M6 2v4H2M10 2v4h4M6 14v-4H2M10 14v-4h4" />
                ) : (
                  <path d="M2 6V2h4M14 6V2h-4M2 10v4h4M14 10v4h-4" />
                )}
              </svg>
            </button>
          </div>
        </div>
      </div>
    </main>
  );
}
