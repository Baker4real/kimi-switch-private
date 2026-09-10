import { readFileSync } from "node:fs";
import { fileURLToPath, URL } from "node:url";
import { defineConfig } from "vite";

/** 打包时把 package.json 的 version 注入前端（issue #132），运行时不读 Tauri API。 */
const pkg = JSON.parse(
  readFileSync(fileURLToPath(new URL("./package.json", import.meta.url)), "utf8"),
) as { version: string };

export default defineConfig({
  root: "src-ui",
  resolve: {
    alias: {
      "@ui": fileURLToPath(new URL("./src-ui", import.meta.url)),
    },
  },
  define: {
    "import.meta.env.VITE_APP_VERSION": JSON.stringify(pkg.version),
  },
  server: {
    host: "127.0.0.1",
    port: 1420,
    strictPort: true,
  },
  preview: {
    host: "127.0.0.1",
    port: 1420,
    strictPort: true,
  },
  build: {
    outDir: "../dist/web",
    emptyOutDir: true,
  },
});
