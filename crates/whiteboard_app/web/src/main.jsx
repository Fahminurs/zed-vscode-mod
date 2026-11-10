// Real Excalidraw 0.18 (same as the prototype), bundled offline and hosted by the Rust shell.
// Rust <-> JS: window.ipc.postMessage(json) and window.__wb.{load, reset, saved}.
import { useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  Excalidraw,
  MainMenu,
  convertToExcalidrawElements,
  getSceneVersion,
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

function App() {
  const api = useRef(null);
  const saved = useRef(0);
  const dirty = useRef(false);
  const [ready, setReady] = useState(false);

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
      appState: { ...data.appState, theme: "dark" },
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
    const y = -st.scrollY + st.height / 2 / z - 90 + (Math.random() * 40 - 20);
    const els = convertToExcalidrawElements(
      [
        {
          type: "rectangle",
          x,
          y,
          width: 220,
          height: 180,
          backgroundColor: "#ffec99",
          fillStyle: "solid",
          strokeColor: "#f08c00",
          strokeWidth: 1,
          roughness: 1,
          roundness: { type: 3 },
          label: {
            text: "Catatan",
            fontSize: 20,
            fontFamily: FONT_FAMILY?.Excalifont ?? 5,
            strokeColor: "#1e1e1e",
          },
        },
      ],
      { regenerateIds: true },
    );
    a.updateScene({ elements: [...a.getSceneElements(), ...els] });
    a.setActiveTool({ type: "selection" });
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
      theme="dark"
      initialData={{ appState: { viewBackgroundColor: "transparent" } }}
      excalidrawAPI={(a) => {
        api.current = a;
        setReady(true);
      }}
      onChange={(elements) => markDirty(getSceneVersion(elements) !== saved.current)}
      onScrollChange={syncDots}
      renderTopRightUI={() => (
        <button className="sticky-btn" onClick={addSticky} title="Add sticky note">
          <span className="sticky-swatch" aria-hidden="true" />
          Sticky note
        </button>
      )}
    >
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
