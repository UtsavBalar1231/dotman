import { useLayoutEffect, useState } from "react";

type Theme = "system" | "light" | "dark";
function themeValue(value: string | null): Theme {
  return value === "light" || value === "dark" ? value : "system";
}
function savedTheme(): Theme {
  try { return themeValue(localStorage.getItem("dotman.theme") ?? "dark"); }
  catch (error) {
    if (error instanceof DOMException) return "dark";
    throw error;
  }
}
export function ThemeSelect() {
  const [theme, setTheme] = useState(savedTheme);
  useLayoutEffect(() => {
    document.documentElement.dataset["theme"] = theme;
    try { localStorage.setItem("dotman.theme", theme); }
    catch (error) {
      if (!(error instanceof DOMException)) throw error;
    }
  }, [theme]);
  return <fieldset className="theme-picker">
    <legend>Color theme</legend>
    {([["dark", "AMOLED"], ["light", "Light"], ["system", "System"]] as const).map(([value, label]) =>
      <label key={value}>
        <input type="radio" name="theme" value={value} checked={value === theme} onChange={() => setTheme(value)} />
        <span className={`theme-swatch theme-${value}`} aria-hidden="true" />
        {label}
      </label>
    )}
  </fieldset>;
}
