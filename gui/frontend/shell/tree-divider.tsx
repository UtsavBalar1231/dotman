import { useLayoutEffect, useRef } from "react";

const minWidth = 220;
const storageKey = "dotman.treeWidth";

export function TreeDivider() {
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const divider = ref.current!;
    const desk = divider.closest<HTMLElement>(".desk")!;
    let preferred = 320;
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
    let dragging: number | undefined;
    divider.onpointerdown = event => {
      if (event.button !== 0) return;
      dragging = event.pointerId;
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
      remember();
    };
    window.addEventListener("pointermove", drag);
    window.addEventListener("pointerup", finish);
    window.addEventListener("pointercancel", finish);
    divider.onlostpointercapture = finish;
    divider.onkeydown = event => {
      if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
      event.preventDefault();
      preferred = Number(divider.getAttribute("aria-valuenow")) + (event.key === "ArrowRight" ? 16 : -16);
      resize();
      remember();
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
    aria-valuemin={minWidth} title="Drag to resize the file tree. Use arrow keys when focused." />;
}
