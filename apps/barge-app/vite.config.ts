import { defineConfig } from "vitest/config";
import { svelte } from "@sveltejs/vite-plugin-svelte";

export default defineConfig({
  plugins: [svelte()],
  resolve: { conditions: ["browser"] },
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  test: { environment: "happy-dom", setupFiles: ["./src/test-setup.ts"] },
});
