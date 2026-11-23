import { useLayoutEffect, useRef } from "react";

const minWidth = 220;
const defaultWidth = 320;
const storageKey = "dotman.treeWidth";

export function TreeDivider() {
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const divider = ref.current!;
    const desk = divider.closest<HTMLElement>(".desk")!;
    let preferred = defaultWidth;
    try {
      const saved = Number(localStorage.getItem(storageKey));
      if (Number.isFinite(saved) && saved >= minWidth) preferred = saved;
    } catch { /* The divider also works without persistent storage. */ }

    function resize() {
      const max = Math.max(minWidth, desk.clientWidth - 380);
      const width = Math.min(max, Math.max(minWidth, preferred));
      desk.style.setProperty("--tree-width", width + "px");
      divider.setAttribute("aria-valuenow", String(width));
      divider.setAttribute("aria-valuemax", String(max));
    }
    function remember() {
      try { localStorage.setItem(storageKey, String(preferred)); }
      catch { /* Keep the width for this window. */ }
    }
    function setWidth(width: number) {
      preferred = width;
      resize();
      remember();
    }
    let dragging: number | undefined;
    divider.onpointerdown = event => {
      if (event.button !== 0) return;
      dragging = event.pointerId;
      divider.dataset.dragging = "true";
      divider.setPointerCapture(event.pointerId);
      event.preventDefault();
      divider.focus();
    };
    const drag = (event: PointerEvent) => {
      if (event.pointerId !== dragging || !event.buttons) return;
      preferred = Math.round(event.clientX - desk.getBoundingClientRect().left);
      resize();
    };
    const finish = (event: PointerEvent) => {
      if (event.pointerId !== dragging) return;
      dragging = undefined;
      delete divider.dataset.dragging;
      remember();
    };
    divider.ondblclick = () => setWidth(defaultWidth);
    window.addEventListener("pointermove", drag);
    window.addEventListener("pointerup", finish);
    window.addEventListener("pointercancel", finish);
    divider.onlostpointercapture = finish;
    divider.onkeydown = event => {
      const current = Number(divider.getAttribute("aria-valuenow"));
      const maximum = Number(divider.getAttribute("aria-valuemax"));
      const step = event.shiftKey ? 64 : 16;
      let width: number;
      switch (event.key) {
        case "ArrowLeft": width = Math.max(minWidth, current - step); break;
        case "ArrowRight": width = Math.min(maximum, current + step); break;
        case "Home": width = minWidth; break;
        case "End": width = maximum; break;
        case "Enter": width = defaultWidth; break;
        default: return;
      }
      event.preventDefault();
      setWidth(width);
    };
    const observer = new ResizeObserver(resize);
    observer.observe(desk);
    resize();
    return () => {
      observer.disconnect();
      window.removeEventListener("pointermove", drag);
      window.removeEventListener("pointerup", finish);
      window.removeEventListener("pointercancel", finish);
    };
  }, []);
  return <div ref={ref} className="tree-divider" tabIndex={0} role="separator"
    aria-controls="entries" aria-label="Resize file tree" aria-orientation="vertical"
    aria-valuemin={minWidth} title="Drag or use arrow keys to resize. Double-click or press Enter to reset." />;
}
