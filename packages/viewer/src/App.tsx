import { createSignal, onMount } from "solid-js";
import { startViewer } from "./viewer.ts";

export default function App() {
  const [status, setStatus] = createSignal("Initializing...");
  let videoRef!: HTMLVideoElement;

  onMount(() => {
    void startViewer(videoRef, setStatus);
  });

  return (
    <main class="flex min-h-screen flex-col items-center justify-center gap-3 p-4">
      <div class="text-sm text-zinc-400">{status()}</div>
      <video
        ref={videoRef}
        controls
        autoplay
        playsinline
        muted
        class="max-h-[90vh] w-full max-w-6xl rounded-lg bg-black"
      />
    </main>
  );
}
