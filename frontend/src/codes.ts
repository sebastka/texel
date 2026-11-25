// Cheat and patch codes in the player: the game's codes (DuckStation's
// database, served by the server), chosen in a panel (off when a game
// starts: nothing is remembered), and turned into what the emulation
// worker runs (a `CodeSet`).

import type { FileCode, GameCodes } from "./api";
import { h } from "./dom";
import type { CodeSet, EnabledCode } from "./protocol";

/** What the player chose this session: each enabled code (`p:` patches,
 * `c:` cheats, then its name) and the value for its `?` digits. Not kept:
 * every game starts with none. */
type Choices = Record<string, number>;

/** Fills each line's run of `?` digits with `value`, in hexadecimal (as the
 * core's `fill`). */
function fill(body: string, value: number): string {
  return body
    .split("\n")
    .map((line) => {
      const m = /\?+/.exec(line);
      if (!m) return line;
      const width = m[0].length;
      const mask = width >= 8 ? 0xffffffff : 2 ** (4 * width) - 1;
      const digits = ((value >>> 0) & mask).toString(16).toUpperCase().padStart(width, "0");
      return line.slice(0, m.index) + digits + line.slice(m.index + width);
    })
    .join("\n");
}

/** Why a code cannot run here, if it cannot. */
function unsupported(code: FileCode): string | null {
  if (code.kind !== "Gameshark") return `${code.kind} codes are not supported`;
  if ((code.overrides.ramMib ?? 2) > 8) return `needs ${code.overrides.ramMib} MiB of RAM (8 at most here)`;
  return null;
}

/** A code's default value: its first choice, or its range's start. */
function defaultValue(code: FileCode): number {
  return code.options[0]?.[1] ?? code.range?.[0] ?? 0;
}

/** What the worker runs for these choices. */
export function codeSet(codes: GameCodes, choices: Choices): CodeSet | null {
  const enabled: EnabledCode[] = [];
  let overclock = 100;
  let ramMiB = 2;
  let wide = false;
  let noGteWidescreen = false;
  for (const [prefix, list] of [
    ["p:", codes.patches],
    ["c:", codes.cheats],
  ] as const) {
    for (const code of list) {
      const value = choices[prefix + code.name];
      if (value === undefined || unsupported(code)) continue;
      enabled.push({ name: prefix + code.name, body: fill(code.body, value), activation: code.activation });
      // Settings: patches' only.
      if (prefix === "p:") {
        overclock = Math.max(overclock, code.overrides.cpuOverclock ?? 100);
        ramMiB = Math.max(ramMiB, code.overrides.ramMib ?? 2);
        wide ||= code.overrides.aspectRatio === "16:9";
        noGteWidescreen ||= code.overrides.disableWidescreen;
      }
    }
  }
  return enabled.length ? { codes: enabled, overclock, ramMiB, wide, noGteWidescreen } : null;
}

/** The codes panel: `onChange` gets the new set (null: none) whenever the
 * choices change. `refused`: codes the core refused, shown as such. */
export function codesPanel(codes: GameCodes, onChange: (set: CodeSet | null) => void) {
  const choices: Choices = {};
  const panel = h("div.codes-panel", { hidden: true });
  const reasons = new Map<string, string>();
  const changed = () => onChange(codeSet(codes, choices));

  const row = (prefix: string, code: FileCode) => {
    const id = prefix + code.name;
    const label = code.name.split("\\").pop() ?? code.name;
    const why = unsupported(code) ?? reasons.get(id) ?? null;
    const box = h("input", { type: "checkbox", checked: choices[id] !== undefined, disabled: !!why }) as HTMLInputElement;
    // Its choice, when it has one: a list, or a number in a range.
    let input: HTMLSelectElement | HTMLInputElement | null = null;
    if (code.options.length) {
      input = h(
        "select",
        { disabled: !!why },
        ...code.options.map(([name, v]) => h("option", { value: String(v), selected: choices[id] === v }, name)),
      ) as HTMLSelectElement;
    } else if (code.range) {
      input = h("input.code-value", {
        type: "number",
        min: String(code.range[0]),
        max: String(code.range[1]),
        value: String(choices[id] ?? code.range[0]),
        disabled: !!why,
      }) as HTMLInputElement;
    }
    const value = () => (input ? Number(input.value) : 0);
    box.onchange = () => {
      if (box.checked) choices[id] = input ? value() : defaultValue(code);
      else delete choices[id];
      changed();
    };
    if (input)
      input.onchange = () => {
        if (choices[id] === undefined) return;
        choices[id] = value();
        changed();
      };
    const settings = [
      code.overrides.cpuOverclock ? `CPU ${code.overrides.cpuOverclock}%` : null,
      code.overrides.ramMib && code.overrides.ramMib > 2 ? `${code.overrides.ramMib} MiB of RAM` : null,
      code.overrides.aspectRatio ? code.overrides.aspectRatio : null,
    ].filter(Boolean);
    return h(
      "label.code-row",
      { title: [code.description, code.author ? `By ${code.author}` : null].filter(Boolean).join("\n") || undefined },
      box,
      h("span.code-name", {}, label),
      input,
      settings.length ? h("span.muted.small", {}, settings.join(" · ")) : null,
      why ? h("span.small.code-why", {}, `Not available: ${why}`) : null,
    );
  };

  /** A section: its codes, in their groups (`Group\Code`). */
  const section = (title: string, prefix: string, list: FileCode[]) => {
    if (!list.length) return null;
    const groups = new Map<string, FileCode[]>();
    for (const c of list) {
      const group = c.name.includes("\\") ? c.name.slice(0, c.name.lastIndexOf("\\")) : "";
      groups.set(group, [...(groups.get(group) ?? []), c]);
    }
    const body = h("div.code-list");
    const render = (filter: string) => {
      body.replaceChildren(
        ...[...groups].flatMap(([group, items]): HTMLElement[] => {
          const shown = items.filter((c) => !filter || c.name.toLowerCase().includes(filter));
          if (!shown.length) return [];
          const rows = shown.map((c) => row(prefix, c));
          return group
            ? [h("details.code-group", { open: !!filter }, h("summary", {}, group.replaceAll("\\", " › ")), ...rows)]
            : rows;
        }),
      );
    };
    render("");
    const search =
      list.length > 20
        ? (h("input.code-search", {
            type: "search",
            placeholder: `Filter ${list.length} ${title.toLowerCase()}`,
            oninput: (e: Event) => render((e.target as HTMLInputElement).value.trim().toLowerCase()),
          }) as HTMLInputElement)
        : null;
    return h("section.code-section", {}, h("h3", {}, title), search, body);
  };

  const render = () => {
    const sections = [section("Patches", "p:", codes.patches), section("Cheats", "c:", codes.cheats)].filter(
      (s): s is HTMLElement => s !== null,
    );
    panel.replaceChildren(
      h(
        "p.muted.small",
        {},
        "From DuckStation's cheat and patch database: each code is its author's. Codes change the game as it runs; " +
          "patches may also make the emulated CPU faster or its memory larger.",
      ),
      ...sections,
    );
  };
  render();
  return {
    panel,
    /** Codes the core refused: shown unavailable, and turned off. */
    refused(list: [string, string][]) {
      for (const [name, why] of list) {
        reasons.set(name, why);
        delete choices[name];
      }
      if (list.length) render();
    },
    /** Locked (a netplay room): shown, not changed. */
    lock(locked: boolean) {
      for (const el of panel.querySelectorAll<HTMLInputElement | HTMLSelectElement>("input, select")) {
        if (el.classList.contains("code-search")) continue;
        el.disabled = locked || el.closest(".code-row")?.querySelector(".code-why") !== null;
      }
    },
  };
}
