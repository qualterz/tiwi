import { defineConfig } from "vite";
import solidPlugin from "vite-plugin-solid";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  base: "./",
  plugins: [solidPlugin(), tailwindcss()],
  server: {
    host: true,
    port: 3000,
  },
});
