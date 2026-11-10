// Real Excalidraw 0.18 (same as the prototype), bundled offline and hosted by the Rust shell.
// Rust <-> JS: window.ipc.postMessage(json) and window.__wb.{load, reset, saved}.
import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { createRoot } from "react-dom/client";
import {
  Excalidraw,
  MainMenu,
  convertToExcalidrawElements,
  getSceneVersion,
  newElementWith,
  restore,
  serializeAsJSON,
  FONT_FAMILY,
} from "@excalidraw/excalidraw";
import "@excalidraw/excalidraw/index.css";

const post = (msg) => window.ipc?.postMessage(JSON.stringify(msg));
const root = document.getElementById("root");

// Dotted background that follows the canvas scroll/zoom.
function syncDots(scrollX, scrollY, zoom) {
  const z = typeof zoom === "number" ? zoom : zoom?.value ?? 1;
  const step = 20 * z;
  root.style.backgroundSize = `${step}px ${step}px`;
  root.style.backgroundPosition = `${scrollX * z}px ${scrollY * z}px`;
}

// Our own chrome (page bg, dots, sticky corner) follows Excalidraw's theme via CSS vars.
const setTheme = (theme) => (document.documentElement.dataset.theme = theme);
setTheme("dark");

/**
 * How Excalidraw shows a canvas colour: as-is in light mode, through its dark-mode filter
 * `invert(93%) hue-rotate(180deg)` in dark mode (so #ffffff shows as #121212).
 */
function shownColor(hex, theme) {
  const m = /^#([0-9a-f]{6})$/i.exec(hex ?? "");
  if (!m || theme !== "dark") return hex;
  const n = parseInt(m[1], 16);
  const [r, g, b] = [n >> 16, (n >> 8) & 255, n & 255].map((v) => {
    const c = v / 255;
    return c + 0.93 * (1 - 2 * c);
  });
  const out = [
    -0.574 * r + 1.43 * g + 0.144 * b,
    0.426 * r + 0.43 * g + 0.144 * b,
    0.426 * r + 1.43 * g - 0.856 * b,
  ];
  return "#" + out.map((v) => Math.round(Math.min(1, Math.max(0, v)) * 255).toString(16).padStart(2, "0")).join("");
}

// Toolbar shortcut labels as letters (V, R, D, …) taken from each tool's title "Name — R or 2".
function letterKeys() {
  for (const label of document.querySelectorAll(".App-toolbar-container label.ToolIcon[title]")) {
    const key = /—\s*([^\s]+)/.exec(label.title)?.[1];
    const span = label.querySelector(".ToolIcon__keybinding");
    if (key && span && span.textContent !== key) span.textContent = key;
  }
}

const NOTE_ICON = (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
    <path d="M13 20l7 -7" />
    <path d="M13 20v-6a1 1 0 0 1 1 -1h6v-7a2 2 0 0 0 -2 -2h-12a2 2 0 0 0 -2 2v12a2 2 0 0 0 2 2h7" />
  </svg>
);

/** Sticky-note tool injected into Excalidraw's toolbar, just before the eraser. */
function StickyTool({ onAdd }) {
  const [host, setHost] = useState(null);
  useEffect(() => {
    const place = () => {
      letterKeys();
      const eraser = document.querySelector('.App-toolbar-container label.ToolIcon[title^="Eraser"]');
      if (!eraser) return;
      let h = document.getElementById("sticky-tool-host");
      if (!h) {
        h = document.createElement("span");
        h.id = "sticky-tool-host";
        h.style.display = "contents";
        setHost(h);
      }
      if (h.nextSibling !== eraser) eraser.parentNode.insertBefore(h, eraser);
    };
    place();
    const obs = new MutationObserver(place);
    obs.observe(document.body, { childList: true, subtree: true });
    return () => obs.disconnect();
  }, []);
  if (!host) return null;
  return createPortal(
    <label className="ToolIcon Shape ToolIcon_size_medium" title="Sticky note — N" onClick={onAdd}>
      <div className="ToolIcon__icon">
        {NOTE_ICON}
        <span className="ToolIcon__keybinding">N</span>
      </div>
    </label>,
    host,
  );
}

// Sticky notes are real canvas elements (so Layers, move, resize, undo and export all work):
// a paper *image* (Excalidraw keeps images true-colour in dark mode, so the paper stays yellow)
// grouped with a transparent rectangle whose bound text is the note.
const STICKY_COLORS = ["#fff3a3", "#ffc9de", "#c3f5c3", "#bfe3ff"];
const NOTE = 220;
const PAD = 10; // room for the paper's shadow inside the image
const STRIP = 22;
const paperFileId = (color) => "sticky-paper-" + color.slice(1);
// Dark mode inverts canvas text, so the ink is picked per theme to always look dark grey.
const noteInk = (theme) => (theme === "dark" ? "#e2e2e2" : "#2b2b2b");

const paperUrls = {};
function paperUrl(color) {
  if (paperUrls[color]) return paperUrls[color];
  const scale = 2;
  const size = NOTE + PAD * 2;
  const c = document.createElement("canvas");
  c.width = c.height = size * scale;
  const g = c.getContext("2d");
  g.scale(scale, scale);
  const [x, y, w, h, fold] = [PAD, PAD, NOTE, NOTE, 18];
  const sheet = () => {
    g.beginPath();
    g.moveTo(x, y);
    g.lineTo(x + w, y);
    g.lineTo(x + w, y + h);
    g.lineTo(x + fold, y + h);
    g.lineTo(x, y + h - fold);
    g.closePath();
  };
  g.save();
  g.shadowColor = "rgba(0, 0, 0, 0.45)";
  g.shadowBlur = 9;
  g.shadowOffsetY = 4;
  sheet();
  g.fillStyle = color;
  g.fill();
  g.restore();
  g.fillStyle = "rgba(0, 0, 0, 0.06)";
  g.fillRect(x, y, w, STRIP);
  g.beginPath();
  g.moveTo(x, y + h - fold);
  g.lineTo(x + fold, y + h - fold);
  g.lineTo(x + fold, y + h);
  g.closePath();
  g.fillStyle = "rgba(0, 0, 0, 0.18)";
  g.fill();
  return (paperUrls[color] = c.toDataURL("image/png"));
}

function ensurePaper(api, color) {
  const id = paperFileId(color);
  if (!api.getFiles()[id]) {
    api.addFiles([{ id, dataURL: paperUrl(color), mimeType: "image/png", created: Date.now() }]);
  }
  return id;
}

const isPaper = (el) => el?.type === "image" && el.customData?.stickyPaper;

/** Recolour the note ink for the theme (dark mode would otherwise show it light on yellow). */
function syncInk(api, theme) {
  const els = api.getSceneElementsIncludingDeleted();
  const noteBoxes = new Set(els.filter((e) => e.customData?.stickyBox).map((e) => e.id));
  const ink = noteInk(theme);
  if (!els.some((e) => noteBoxes.has(e.containerId) && e.strokeColor !== ink)) return;
  api.updateScene({
    elements: els.map((e) =>
      noteBoxes.has(e.containerId) && e.strokeColor !== ink ? newElementWith(e, { strokeColor: ink }) : e,
    ),
    captureUpdate: "NEVER",
  });
}

/** Colour dots shown above a selected sticky note. */
function StickyPalette({ at, onPick }) {
  if (!at) return null;
  return (
    <div className="sticky-palette" style={{ left: at.x, top: at.y }}>
      {STICKY_COLORS.map((c) => (
        <button key={c} style={{ background: c }} title="Note colour" onClick={() => onPick(at.gid, c)} />
      ))}
    </div>
  );
}

function App() {
  const api = useRef(null);
  const saved = useRef(0);
  const dirty = useRef(false);
  const [ready, setReady] = useState(false);
  const [palette, setPalette] = useState(null);
  // Excalidraw's own canvas stays transparent so the dots always show; the chosen canvas
  // colour is painted underneath them (and written back into the file on save).
  const canvasColor = useRef(null);
  const paintCanvas = (color, theme = document.documentElement.dataset.theme) => {
    canvasColor.current = color;
    const css = document.documentElement.style;
    color ? css.setProperty("--wb-bg", shownColor(color, theme)) : css.removeProperty("--wb-bg");
  };

  const markDirty = (value) => {
    if (dirty.current !== value) {
      dirty.current = value;
      post({ type: "dirty", dirty: value });
    }
  };

  const load = (text) => {
    const data = restore(JSON.parse(text), null, null);
    const a = api.current;
    const bg = data.appState?.viewBackgroundColor;
    paintCanvas(bg && bg !== "transparent" ? bg : null);
    a.updateScene({
      elements: data.elements,
      appState: {
        ...data.appState,
        viewBackgroundColor: "transparent",
        theme: a.getAppState().theme,
      },
      captureUpdate: "NEVER",
    });
    if (data.files) a.addFiles(Object.values(data.files));
    syncInk(a, a.getAppState().theme);
    a.history?.clear?.();
    saved.current = getSceneVersion(data.elements);
    markDirty(false);
    const s = a.getAppState();
    syncDots(s.scrollX, s.scrollY, s.zoom);
    requestAnimationFrame(() => a.scrollToContent(undefined, { fitToViewport: false }));
  };

  const save = (saveAs) => {
    const a = api.current;
    const appState = { ...a.getAppState(), viewBackgroundColor: canvasColor.current ?? "transparent" };
    const data = serializeAsJSON(a.getSceneElements(), appState, a.getFiles(), "local");
    post({ type: "save", data, saveAs });
  };

  const addSticky = () => {
    const a = api.current;
    const st = a.getAppState();
    const z = st.zoom.value;
    const x = -st.scrollX + st.width / 2 / z - NOTE / 2 + (Math.random() * 40 - 20);
    const y = -st.scrollY + st.height / 2 / z - NOTE / 2 + (Math.random() * 40 - 20);
    const color = STICKY_COLORS[0];
    const fileId = ensurePaper(a, color);
    const gid = "sticky-" + Math.random().toString(36).slice(2, 10);
    const els = convertToExcalidrawElements(
      [
        {
          type: "image",
          fileId,
          x: x - PAD,
          y: y - PAD,
          width: NOTE + PAD * 2,
          height: NOTE + PAD * 2,
          groupIds: [gid],
          customData: { stickyPaper: true, color },
        },
        {
          type: "rectangle",
          x: x + 8,
          y: y + STRIP + 4,
          width: NOTE - 16,
          height: NOTE - STRIP - 12,
          strokeColor: "transparent",
          backgroundColor: "transparent",
          roughness: 0,
          roundness: null,
          groupIds: [gid],
          customData: { stickyBox: true },
          label: {
            text: " ",
            fontSize: 20,
            fontFamily: FONT_FAMILY?.Excalifont ?? 5,
            strokeColor: noteInk(st.theme),
            textAlign: "left",
            verticalAlign: "top",
          },
        },
      ],
      { regenerateIds: false },
    );
    a.updateScene({ elements: [...a.getSceneElements(), ...els] });
    a.setActiveTool({ type: "selection" });
    // Select the text box and press Enter for the user, so typing starts right away.
    const box = els.find((e) => e.customData?.stickyBox);
    a.updateScene({ appState: { selectedElementIds: { [box.id]: true }, selectedGroupIds: {} } });
    requestAnimationFrame(() =>
      document
        .querySelector(".excalidraw-container, .excalidraw")
        ?.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", code: "Enter", keyCode: 13, bubbles: true })),
    );
  };

  const recolorSticky = (gid, color) => {
    const a = api.current;
    const fileId = ensurePaper(a, color);
    a.updateScene({
      elements: a
        .getSceneElementsIncludingDeleted()
        .map((e) =>
          isPaper(e) && e.groupIds.includes(gid)
            ? newElementWith(e, { fileId, customData: { ...e.customData, color } })
            : e,
        ),
    });
  };

  useEffect(() => {
    if (!ready) return;
    window.__wb = {
      load,
      reset: () => {
        paintCanvas(null);
        api.current.resetScene();
        saved.current = getSceneVersion(api.current.getSceneElements());
        markDirty(false);
      },
      saved: () => {
        saved.current = getSceneVersion(api.current.getSceneElements());
        markDirty(false);
      },
    };
    fetch("/__file")
      .then((r) => (r.ok ? r.text() : null))
      .then((t) => t && load(t))
      .catch(() => {});
    // App shortcuts win over Excalidraw's own (it would use the browser file APIs).
    const onKey = (e) => {
      const t = e.target;
      const typing = t?.isContentEditable || /^(INPUT|TEXTAREA)$/.test(t?.tagName ?? "");
      if (!e.ctrlKey && !e.altKey && !e.metaKey && !typing && e.key.toLowerCase() === "n") {
        e.preventDefault();
        e.stopPropagation();
        return addSticky();
      }
      if (!e.ctrlKey || e.altKey) return;
      const k = e.key.toLowerCase();
      const msg =
        k === "s" ? (save(e.shiftKey), null) : k === "o" ? { type: "open" } : k === "n" ? { type: "new" } : undefined;
      if (msg === undefined) return;
      e.preventDefault();
      e.stopPropagation();
      if (msg) post(msg);
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [ready]);

  return (
    <>
    <StickyPalette at={palette} onPick={recolorSticky} />
    <Excalidraw
      initialData={{ appState: { viewBackgroundColor: "transparent", theme: "dark" } }}
      excalidrawAPI={(a) => {
        api.current = a;
        setReady(true);
      }}
      onChange={(elements, appState) => {
        markDirty(getSceneVersion(elements) !== saved.current);
        const bg = appState.viewBackgroundColor;
        if (bg && bg !== "transparent") {
          // A canvas colour was picked: paint it under the dots (theme is left alone).
          paintCanvas(bg, appState.theme);
          markDirty(true);
          api.current?.updateScene({ appState: { viewBackgroundColor: "transparent" } });
          return;
        }
        if (document.documentElement.dataset.theme !== appState.theme) {
          setTheme(appState.theme);
          // Same canvas colour, shown the way the new theme shows it.
          if (canvasColor.current) paintCanvas(canvasColor.current, appState.theme);
          if (api.current) syncInk(api.current, appState.theme);
        }
        // Colour dots above a selected sticky note (only when nothing else is selected).
        const sel = elements.filter((e) => appState.selectedElementIds[e.id]);
        const gid = sel[0]?.groupIds?.[0];
        const paper =
          gid && !appState.editingTextElement && sel.every((e) => e.groupIds?.includes(gid))
            ? elements.find((e) => isPaper(e) && e.groupIds.includes(gid))
            : null;
        const z = appState.zoom.value;
        const next = paper
          ? {
              gid,
              x: Math.round((paper.x + paper.width - PAD + appState.scrollX) * z + appState.offsetLeft),
              y: Math.round((paper.y + PAD + appState.scrollY) * z + appState.offsetTop) - 34,
            }
          : null;
        if (JSON.stringify(next) !== JSON.stringify(palette)) setPalette(next);
      }}
      onScrollChange={syncDots}
    >
      <StickyTool onAdd={addSticky} />
      <MainMenu>
        <MainMenu.Item onSelect={() => post({ type: "open" })} shortcut="Ctrl+O">
          Open
        </MainMenu.Item>
        <MainMenu.Item onSelect={() => save(false)} shortcut="Ctrl+S">
          Save
        </MainMenu.Item>
        <MainMenu.Item onSelect={() => save(true)} shortcut="Ctrl+Shift+S">
          Save as…
        </MainMenu.Item>
        <MainMenu.DefaultItems.SaveAsImage />
        <MainMenu.DefaultItems.SearchMenu />
        <MainMenu.DefaultItems.Help />
        <MainMenu.DefaultItems.ClearCanvas />
        <MainMenu.Separator />
        <MainMenu.DefaultItems.ToggleTheme />
        <MainMenu.DefaultItems.ChangeCanvasBackground />
      </MainMenu>
    </Excalidraw>
    </>
  );
}

createRoot(root).render(<App />);
