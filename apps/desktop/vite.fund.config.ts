import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { defineConfig, type Plugin } from "vite";

function web3AuthModalCss(): string {
  const src = readFileSync(
    resolve("node_modules/@web3auth/modal/dist/lib.esm/packages/modal/src/ui/css/index.css.js"),
    "utf8",
  );
  const match = src.match(/var css_\w+ = (".*");\s*styleInject/s);
  if (!match) throw new Error("could not extract Web3Auth modal CSS");
  // @import inside a JS-injected <style> is ignored by the browser, and can
  // keep the rest of the sheet from applying. Fonts are linked from fund.html.
  return (JSON.parse(match[1]) as string).replace(
    /@import\s+"https:\/\/fonts\.googleapis\.com\/[^"]+";/,
    "",
  );
}

/** Write modal CSS to disk and inject it (vite IIFE drops style-inject). */
function cssInjectedByJs(): Plugin {
  return {
    name: "css-injected-by-js",
    apply: "build",
    generateBundle(_opts, bundle) {
      let css = web3AuthModalCss();
      for (const [file, chunk] of Object.entries(bundle)) {
        if (chunk.type === "asset" && chunk.fileName.endsWith(".css")) {
          css += String(chunk.source);
          delete bundle[file];
        }
      }
      writeFileSync(resolve("src-tauri/resources/fund-w3a.css"), css);
      const js = Object.values(bundle).find((c) => c.type === "chunk");
      if (js && js.type === "chunk") {
        js.code =
          `var process=typeof process<"u"?process:{env:{NODE_ENV:"production"}};` +
          `(function(){var s=document.createElement("style");s.textContent=${JSON.stringify(css)};document.head.appendChild(s);})();` +
          js.code;
      }
    },
  };
}

export default defineConfig({
  publicDir: false,
  define: {
    global: "globalThis",
    "process.env.NODE_ENV": JSON.stringify("production"),
  },
  plugins: [cssInjectedByJs()],
  build: {
    emptyOutDir: false,
    outDir: resolve("src-tauri/resources"),
    lib: {
      entry: resolve("src/fund-w3a.ts"),
      name: "RootmodeW3ABundle",
      formats: ["iife"],
      fileName: () => "fund-w3a.js",
    },
    rollupOptions: {
      treeshake: {
        moduleSideEffects: (id) =>
          id.includes("index.css.js") || id.includes("style-inject"),
      },
      output: {
        inlineDynamicImports: true,
        assetFileNames: "fund-w3a.[ext]",
      },
    },
    cssCodeSplit: false,
    target: "es2021",
    sourcemap: false,
  },
});
