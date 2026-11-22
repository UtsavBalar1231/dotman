import { fluentPaths } from "../fluent-icons.js";
import type { IconName } from "../fluent-icons.js";

export function Icon({ name, className = "" }: { name: IconName; className?: string }) {
  return <svg className={`workspace-glyph ${className}`} aria-hidden="true" focusable="false"><use href={`#fluent-${name}`}/></svg>;
}

export function Symbols() {
  return (
  <svg xmlns="http://www.w3.org/2000/svg" width="0" height="0" className="workspace-symbols" aria-hidden="true" focusable="false">
    <defs>
      <symbol id="pair-mark" viewBox="0 0 64 64">
        <path className="mark-back" d="m45 13 15-7-7 40c-.8 4.4-.4 7 1.5 9L43 60c-2-1.5-3-4-3-7-4.2 4.6-9 6.8-14 6-9.2-1.3-12.6-9.6-10-20.4C19.2 25.7 29 18.4 42.8 21ZM4.4 49.4c1-3.4 4.5-5.6 8.3-5.8l2.2 5.8c-1.4 4-5.1 6.2-9.7 6.3Z"/>
        <path className="mark-front" fillRule="evenodd" d="m43 10 15-7-7 40c-.8 4.4-.4 7 1.5 9L41 57c-2-1.5-3-4-3-7-4.2 4.6-9 6.8-14 6-9.2-1.3-12.6-9.6-10-20.4C17.2 22.7 27 15.4 40.8 18Zm-19 32c3.5-4.7 6-6.2 9.8-4.2 3.8 2 6.7-.3 8.6-4.3l1-7c-3.2 4.5-5.9 5.7-9.8 3.8-4-1.9-7.2.9-8.6 5.7Z"/>
        <path className="mark-dot" d="M2.4 46.4c1-3.4 4.5-5.6 8.3-5.8l2.2 5.8c-1.4 4-5.1 6.2-9.7 6.3Z"/>
      </symbol>
      {Object.entries(fluentPaths).map(([name, path]) => (
        <symbol id={`fluent-${name}`} key={name} viewBox="0 0 24 24">
          <path d={path}/>
        </symbol>
      ))}
    </defs>
  </svg>

  );
}
