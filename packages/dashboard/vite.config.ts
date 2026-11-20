import { defineConfig } from "vite";
import solidPlugin from "vite-plugin-solid";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  plugins: [solidPlugin(), tailwindcss()],
  server: {
    port: 5173,
    proxy: {
      "/ws": {
        target: "ws://127.0.0.1:2000",
        ws: true,
      },
    },
  },
});
