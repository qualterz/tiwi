import { createSignal } from "solid-js";

export default function App() {
  const [status] = createSignal("Connecting to daemon...");

  return (
    <main class="mx-auto max-w-4xl p-6">
      <h1 class="text-2xl font-semibold tracking-tight">tiwi dashboard</h1>
      <p class="mt-2 text-sm text-zinc-400">{status()}</p>
    </main>
  );
}
