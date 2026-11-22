import { useLayoutEffect, useState } from "react";
import { Icon } from "./symbols.js";

type Theme = "system" | "light" | "dark";
function themeValue(value: string | null): Theme {
  return value === "light" || value === "dark" ? value : "system";
}
function savedTheme(): Theme {
  try { return themeValue(localStorage.getItem("dotman.theme")); }
  catch (error) {
    if (error instanceof DOMException) return "system";
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
  return <label>
    Theme
    <span className="theme-field">
      <select id="theme-mode" value={theme} onChange={(event) => setTheme(themeValue(event.currentTarget.value))}>
        <option value="system">System</option><option value="light">Light</option><option value="dark">Dark</option>
      </select>
      <Icon name="chevron-down" />
    </span>
  </label>;
}
