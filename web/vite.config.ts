import { fileURLToPath } from "node:url";
import { defineConfig, type Plugin } from "vite";
import solidPlugin from "vite-plugin-solid";
import tailwindcss from "@tailwindcss/vite";

function redirectHomeSlash(): Plugin {
  const middleware = (
    req: { url?: string },
    res: {
      statusCode: number;
      setHeader: (name: string, value: string) => void;
      end: () => void;
    },
    next: () => void,
  ) => {
    if (req.url === "/home" || req.url?.startsWith("/home?")) {
      res.statusCode = 302;
      res.setHeader("Location", req.url.replace(/^\/home/, "/home/"));
      res.end();
      return;
    }
    next();
  };

  return {
    name: "redirect-home-slash",
    configureServer(server) {
      server.middlewares.use(middleware);
    },
    configurePreviewServer(server) {
      server.middlewares.use(middleware);
    },
  };
}

export default defineConfig({
  appType: "mpa",
  base: "./",
  plugins: [solidPlugin(), tailwindcss(), redirectHomeSlash()],
  build: {
    rollupOptions: {
      input: {
        main: fileURLToPath(new URL("./index.html", import.meta.url)),
        home: fileURLToPath(new URL("./home/index.html", import.meta.url)),
      },
    },
  },
  server: {
    host: true,
    port: 3000,
  },
});
