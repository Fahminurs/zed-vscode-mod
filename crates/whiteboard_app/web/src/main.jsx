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

/** Light canvas colour → light theme, dark colour → dark theme ("transparent" keeps it). */
function themeFor(bg) {
  const m = /^#([0-9a-f]{6})/i.exec(bg ?? "");
  if (!m) return null;
  const n = parseInt(m[1], 16);
  const lum = (0.299 * (n >> 16) + 0.587 * ((n >> 8) & 255) + 0.114 * (n & 255)) / 255;
  return lum > 0.5 ? "light" : "dark";
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

// Sticky notes are embeddables rendered as HTML (not inverted by the dark canvas filter),
// so they stay real paper-yellow. Text/colour live in element.customData.
const STICKY_LINK = "sticky:note";
const STICKY_COLORS = ["#fff3a3", "#ffc9de", "#c3f5c3", "#bfe3ff"];
const isSticky = (link) => typeof link === "string" && link.startsWith("sticky:");

function updateSticky(api, id, patch) {
  const els = api.getSceneElementsIncludingDeleted();
  api.updateScene({
    elements: els.map((e) =>
      e.id === id ? newElementWith(e, { customData: { ...e.customData, ...patch } }) : e,
    ),
  });
}

function StickyNote({ el, api, active }) {
  const text = useRef(null);
  const data = el.customData ?? {};
  useEffect(() => {
    const node = text.current;
    if (!active || !node) return;
    node.focus();
    const range = document.createRange();
    range.selectNodeContents(node);
    range.collapse(false);
    const sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(range);
  }, [active]);
  useEffect(() => {
    const node = text.current;
    if (node && document.activeElement !== node && node.innerText !== (data.text ?? "")) {
      node.innerText = data.text ?? "";
    }
  }, [data.text]);
  return (
    <div className="sticky-paper" style={{ background: data.color ?? STICKY_COLORS[0] }}>
      <div className="sticky-colors">
        {STICKY_COLORS.map((c) => (
          <button
            key={c}
            style={{ background: c }}
            title="Note colour"
            onPointerDown={(e) => e.stopPropagation()}
            onClick={() => updateSticky(api, el.id, { color: c })}
          />
        ))}
      </div>
      <div
        ref={text}
        className="sticky-text"
        contentEditable
        suppressContentEditableWarning
        spellCheck={false}
        data-placeholder="Tulis catatan…"
        onKeyDown={(e) => e.stopPropagation()}
        onBlur={(e) => updateSticky(api, el.id, { text: e.currentTarget.innerText })}
      />
    </div>
  );
}

function App() {
  const api = useRef(null);
  const saved = useRef(0);
  const dirty = useRef(false);
  const [ready, setReady] = useState(false);
  const lastBg = useRef("transparent");

  const markDirty = (value) => {
    if (dirty.current !== value) {
      dirty.current = value;
      post({ type: "dirty", dirty: value });
    }
  };

  const load = (text) => {
    const data = restore(JSON.parse(text), null, null);
    const a = api.current;
    a.updateScene({
      elements: data.elements,
      appState: { ...data.appState, theme: a.getAppState().theme },
      captureUpdate: "NEVER",
    });
    if (data.files) a.addFiles(Object.values(data.files));
    a.history?.clear?.();
    saved.current = getSceneVersion(data.elements);
    markDirty(false);
    const s = a.getAppState();
    syncDots(s.scrollX, s.scrollY, s.zoom);
    requestAnimationFrame(() => a.scrollToContent(undefined, { fitToViewport: false }));
  };

  const save = (saveAs) => {
    const a = api.current;
    const data = serializeAsJSON(a.getSceneElements(), a.getAppState(), a.getFiles(), "local");
    post({ type: "save", data, saveAs });
  };

  const addSticky = () => {
    const a = api.current;
    const st = a.getAppState();
    const z = st.zoom.value;
    const x = -st.scrollX + st.width / 2 / z - 110 + (Math.random() * 40 - 20);
    const y = -st.scrollY + st.height / 2 / z - 110 + (Math.random() * 40 - 20);
    const [base] = convertToExcalidrawElements(
      [{ type: "rectangle", x, y, width: 220, height: 220 }],
      { regenerateIds: true },
    );
    const note = {
      ...base,
      type: "embeddable",
      link: STICKY_LINK,
      strokeColor: "transparent",
      backgroundColor: "transparent",
      strokeWidth: 1,
      roughness: 0,
      roundness: null,
      customData: { sticky: true, text: "", color: STICKY_COLORS[0] },
    };
    a.updateScene({ elements: [...a.getSceneElements(), note] });
    a.setActiveTool({ type: "selection" });
    // Open it for typing straight away.
    const placed = a.getSceneElements().find((e) => e.id === note.id);
    a.updateScene({
      appState: {
        selectedElementIds: { [note.id]: true },
        activeEmbeddable: placed ? { element: placed, state: "active" } : null,
      },
    });
  };

  useEffect(() => {
    if (!ready) return;
    window.__wb = {
      load,
      reset: () => {
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
    <Excalidraw
      initialData={{ appState: { viewBackgroundColor: "transparent", theme: "dark" } }}
      excalidrawAPI={(a) => {
        api.current = a;
        setReady(true);
      }}
      onChange={(elements, appState) => {
        markDirty(getSceneVersion(elements) !== saved.current);
        // Picking a light/dark canvas colour switches the whole UI to match.
        if (appState.viewBackgroundColor !== lastBg.current) {
          lastBg.current = appState.viewBackgroundColor;
          const want = themeFor(appState.viewBackgroundColor);
          if (want && want !== appState.theme) {
            api.current?.updateScene({ appState: { theme: want } });
          }
        }
        if (document.documentElement.dataset.theme !== appState.theme) {
          setTheme(appState.theme);
          // Back to dark with a light canvas colour: drop it so the dotted dark canvas shows.
          if (appState.theme === "dark" && themeFor(appState.viewBackgroundColor) === "light") {
            lastBg.current = "transparent";
            api.current?.updateScene({ appState: { viewBackgroundColor: "transparent" } });
          }
        }
      }}
      onScrollChange={syncDots}
      validateEmbeddable={(link) => (isSticky(link) ? true : undefined)}
      renderEmbeddable={(el, appState) =>
        isSticky(el.link) ? (
          <StickyNote
            el={el}
            api={api.current}
            active={appState.activeEmbeddable?.element?.id === el.id && appState.activeEmbeddable?.state === "active"}
          />
        ) : null
      }
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
  );
}

createRoot(root).render(<App />);
