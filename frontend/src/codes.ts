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

/** The codes panel: tabs for patches and cheats, each a grid of cards (by
 * group) with a switch, a description and the code's choice. `onChange`
 * gets the new set (null: none) whenever the choices change. */
export function codesPanel(codes: GameCodes, onChange: (set: CodeSet | null) => void) {
  const choices: Choices = {};
  const reasons = new Map<string, string>();
  let locked = false;
  const tabs = [
    { title: "Patches", prefix: "p:", list: codes.patches },
    { title: "Cheats", prefix: "c:", list: codes.cheats },
  ].filter((t) => t.list.length);
  let tab = tabs[0]!;
  let filter = "";

  const panel = h("div.codes-panel");
  const body = h("div.codes-body");
  const count = h("span.codes-count.muted.small");
  const allOff = h(
    "button.ghost.small",
    {
      onclick: () => {
        for (const k of Object.keys(choices)) delete choices[k];
        changed();
        renderBody();
      },
    },
    "Turn all off",
  ) as HTMLButtonElement;
  const search = h("input.code-search", {
    type: "search",
    placeholder: "Filter",
    oninput: (e: Event) => {
      filter = (e.target as HTMLInputElement).value.trim().toLowerCase();
      renderBody();
    },
  }) as HTMLInputElement;
  const tabButtons = tabs.map((t) =>
    h(
      "button.codes-tab",
      {
        role: "tab",
        onclick: () => {
          tab = t;
          renderBody();
        },
      },
      t.title,
      h("span.codes-tab-count", {}, String(t.list.length)),
    ),
  );

  const changed = () => {
    onChange(codeSet(codes, choices));
    updateHead();
  };
  const updateHead = () => {
    const on = Object.keys(choices).length;
    count.textContent = on ? `${on} on` : "None on";
    allOff.disabled = locked || on === 0;
    tabButtons.forEach((b, k) => b.setAttribute("aria-selected", String(tabs[k] === tab)));
    search.hidden = tab.list.length <= 12;
  };

  const card = (prefix: string, code: FileCode) => {
    const id = prefix + code.name;
    const label = code.name.split("\\").pop() ?? code.name;
    const why = unsupported(code) ?? reasons.get(id) ?? null;
    const on = choices[id] !== undefined;
    const box = h("input.switch", {
      type: "checkbox",
      checked: on,
      disabled: !!why || locked,
      "aria-label": label,
    }) as HTMLInputElement;
    // Its choice, when it has one: a list, or a number in a range.
    let input: HTMLSelectElement | HTMLInputElement | null = null;
    if (code.options.length) {
      input = h(
        "select.code-choice",
        { disabled: !!why || locked },
        ...code.options.map(([name, v]) => h("option", { value: String(v), selected: choices[id] === v }, name)),
      ) as HTMLSelectElement;
    } else if (code.range) {
      input = h("input.code-choice", {
        type: "number",
        min: String(code.range[0]),
        max: String(code.range[1]),
        value: String(choices[id] ?? code.range[0]),
        disabled: !!why || locked,
      }) as HTMLInputElement;
    }
    const value = () => (input ? Number(input.value) : 0);
    const el = h(
      `label.code-card${on ? ".on" : ""}${why ? ".unavailable" : ""}`,
      { title: [code.description, code.author ? `By ${code.author}` : null].filter(Boolean).join("\n") || undefined },
      h("span.code-card-top", {}, h("span.code-name", {}, label), box),
      code.description ? h("span.code-desc", {}, code.description) : null,
      input,
      (() => {
        const meta = [
          code.overrides.cpuOverclock ? `CPU ${code.overrides.cpuOverclock}%` : null,
          code.overrides.ramMib && code.overrides.ramMib > 2 ? `${code.overrides.ramMib} MiB RAM` : null,
          code.overrides.aspectRatio,
          code.author ? `by ${code.author}` : null,
        ].filter(Boolean);
        return meta.length ? h("span.code-meta", {}, meta.join(" · ")) : null;
      })(),
      why ? h("span.code-why", {}, `Not available: ${why}`) : null,
    );
    box.onchange = () => {
      if (box.checked) choices[id] = input ? value() : defaultValue(code);
      else delete choices[id];
      el.classList.toggle("on", box.checked);
      changed();
    };
    if (input)
      input.onchange = () => {
        if (choices[id] === undefined) return;
        choices[id] = value();
        changed();
      };
    return el;
  };

  /** The current tab's codes, in their groups (`Group\\Code`). */
  const renderBody = () => {
    const groups = new Map<string, FileCode[]>();
    for (const c of tab.list) {
      if (filter && !c.name.toLowerCase().includes(filter) && !(c.description ?? "").toLowerCase().includes(filter)) continue;
      const group = c.name.includes("\\") ? c.name.slice(0, c.name.lastIndexOf("\\")) : "";
      groups.set(group, [...(groups.get(group) ?? []), c]);
    }
    const blocks = [...groups].map(([group, items]) =>
      h(
        "section.code-group",
        {},
        group ? h("h4.code-group-title", {}, group.replaceAll("\\", " › ")) : null,
        h("div.code-grid", {}, ...items.map((c) => card(tab.prefix, c))),
      ),
    );
    body.replaceChildren(...(blocks.length ? blocks : [h("p.muted.small", {}, "No code matches.")]));
    updateHead();
  };

  panel.replaceChildren(
    h(
      "div.codes-head",
      {},
      h("div.codes-tabs", { role: "tablist" }, ...tabButtons),
      search,
      h("span.codes-head-end", {}, count, allOff),
    ),
    body,
    h("p.codes-credit.muted.small", {}, "From DuckStation's database: each code is its author's. Patches may make the emulated CPU faster or its memory larger."),
  );
  renderBody();

  return {
    panel,
    /** Codes the core refused: shown unavailable, and turned off. */
    refused(list: [string, string][]) {
      for (const [name, why] of list) {
        reasons.set(name, why);
        delete choices[name];
      }
      if (list.length) renderBody();
    },
    /** Locked (a netplay room): shown, not changed. */
    lock(on: boolean) {
      if (on === locked) return;
      locked = on;
      renderBody();
    },
  };
}
