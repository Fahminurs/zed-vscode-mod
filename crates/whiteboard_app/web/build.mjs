// Bundles Excalidraw + the app into web/dist, which the Rust binary embeds (include_dir).
import { build } from "esbuild";
import { cpSync, mkdirSync, readFileSync, readdirSync, rmSync } from "node:fs";

// Show "Fill" for every shape that has a Background section (rectangle, diamond, ellipse, …),
// not only after a background colour is picked. Fails the build if Excalidraw's code changes.
const alwaysShowFill = {
  name: "always-show-fill",
  setup(b) {
    b.onLoad({ filter: /@excalidraw[\\/]excalidraw[\\/]dist[\\/]prod[\\/]index\.js$/ }, (args) => {
      const src = readFileSync(args.path, "utf8");
      const re = /(\w+)\((\w+),(\w+)\)&&(\w+)\("div",\{children:(\w+)\("changeBackgroundColor"\)\}\),\w+&&\5\("changeFillStyle"\)/;
      if (!re.test(src)) throw new Error("always-show-fill: pattern not found");
      const out = src.replace(re, '$1($2,$3)&&$4("div",{children:$5("changeBackgroundColor")}),$1($2,$3)&&$5("changeFillStyle")');
      return { contents: out, loader: "js" };
    });
  },
};

rmSync("dist", { recursive: true, force: true });
mkdirSync("dist/fonts", { recursive: true });

await build({
  entryPoints: ["src/main.jsx"],
  bundle: true,
  splitting: true,
  format: "esm",
  outdir: "dist",
  chunkNames: "chunks/[name]-[hash]",
  assetNames: "assets/[name]-[hash]",
  minify: true,
  jsx: "automatic",
  conditions: ["production"],
  plugins: [alwaysShowFill],
  loader: { ".woff2": "file", ".png": "file", ".svg": "file" },
  define: {
    "process.env.NODE_ENV": '"production"',
    "process.env.IS_PREACT": '"false"',
  },
});

cpSync("src/index.html", "dist/index.html");
// Excalidraw loads its fonts from EXCALIDRAW_ASSET_PATH + "fonts/...". Xiaolai (CJK, 13 MB) is skipped.
const fonts = "node_modules/@excalidraw/excalidraw/dist/prod/fonts";
for (const f of readdirSync(fonts)) {
  if (f !== "Xiaolai") cpSync(`${fonts}/${f}`, `dist/fonts/${f}`, { recursive: true });
}
