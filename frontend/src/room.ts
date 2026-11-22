// The netplay room, beside the game: its code, its members and who has
// which controller, and the chat.

import { h, type Child } from "./dom";
import type { ChatLine, Half, Member, NetStatus, Ports, SplitView } from "./protocol";

const NAME_KEY = "texel.name";

/** The signed-in user's name: rooms are opened under it. */
let accountName: string | null = null;

export function setAccountName(name: string | null) {
  accountName = name;
}

export function savedName(): string {
  try {
    return localStorage.getItem(NAME_KEY) ?? "";
  } catch {
    return "";
  }
}

export function saveName(name: string) {
  try {
    localStorage.setItem(NAME_KEY, name);
  } catch {
    // Private mode: asked again next time.
  }
}

/** "ABCDEF" → "ABC-DEF". */
export const formatCode = (code: string) => `${code.slice(0, 3)}-${code.slice(3)}`;

export interface RoomOptions {
  /** Joining someone's room, rather than hosting. */
  guest: boolean;
  /** Host: opens a room under this name. */
  open: (name: string) => void;
  setPorts: (ports: Ports) => void;
  /** Host: split screen. */
  setView: (view: SplitView) => void;
  say: (text: string) => void;
  leave: () => void;
}

export interface RoomPanel {
  element: HTMLElement;
  status: (s: NetStatus) => void;
  chat: (line: ChatLine) => void;
}

const time = (ms: number) => {
  const d = new Date(ms);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
};

export function roomPanel(o: RoomOptions): RoomPanel {
  let net: NetStatus | null = null;
  const body = h("div.room-body");
  const log = h("div.chat-log", { role: "log", "aria-live": "polite" });
  const message = h("input.chat-input", { placeholder: "Message", maxLength: 500, "aria-label": "Chat message" });
  const chat = h(
    "section.chat",
    { hidden: true },
    h("h3", {}, "Chat"),
    log,
    h(
      "form.chat-form",
      {
        onsubmit: (e: Event) => {
          e.preventDefault();
          const text = message.value.trim();
          if (!text) return;
          o.say(text);
          message.value = "";
        },
      },
      message,
      h("button.ghost", { type: "submit" }, "Send"),
    ),
  );
  // Split screen: built once, so that an open menu survives updates.
  const VIEWS: [SplitView, string][] = [
    [null, "Everyone sees the whole screen"],
    ["top", "Controller 1 sees the top half, controller 2 the bottom"],
    ["bottom", "Controller 1 sees the bottom half, controller 2 the top"],
    ["left", "Controller 1 sees the left half, controller 2 the right"],
    ["right", "Controller 1 sees the right half, controller 2 the left"],
  ];
  const viewSelect = h(
    "select.view-select",
    {
      "aria-label": "Split screen",
      onchange: (e: Event) => {
        const v = (e.target as HTMLSelectElement).value;
        o.setView(v ? (v as Half) : null);
      },
    },
    ...VIEWS.map(([v, label]) => h("option", { value: v ?? "" }, label)),
  );
  const viewText = h("p.small");
  const split = h(
    "section.split",
    { hidden: true },
    h("h3", {}, "Split screen"),
    o.guest ? viewText : viewSelect,
    h(
      "p.muted.small",
      {},
      "For split-screen games: each player's browser hides the other player's half. " +
        "Those who watch, or hold both controllers, see everything.",
    ),
  );
  const element = h("aside.room", {}, h("h2", {}, "Play together"), body, split, chat);

  const active = () => !!net && net.state !== "ended" && net.state !== "connecting";

  const chips = (m: Member, ports: Ports) =>
    h(
      "span.chips",
      {},
      ...[0, 1].map((port) => {
        const on = ports[port]!.includes(m.id);
        const label = `Controller ${port + 1}`;
        if (o.guest) return on ? h("span.chip.on", { title: label }, `🎮 ${port + 1}`) : null;
        return h(
          `button.chip${on ? ".on" : ""}`,
          {
            title: m.playing ? `${on ? "Take back" : "Give"} ${label.toLowerCase()}` : "Still joining",
            disabled: !m.playing,
            "aria-pressed": String(on),
            onclick: () => {
              const next = ports.map((ids, i) =>
                i === port ? (on ? ids.filter((x) => x !== m.id) : [...ids, m.id]) : [...ids],
              ) as Ports;
              o.setPorts(next);
            },
          },
          `🎮 ${port + 1}`,
        );
      }),
    );

  const render = () => {
    const s = net;
    const parts: Child[] = [];
    if (!o.guest && (!s || s.state === "ended")) {
      const name = h("input.name-input", {
        value: accountName ?? savedName(),
        placeholder: "Your name",
        maxLength: 24,
        "aria-label": "Your name",
        readOnly: accountName !== null,
        title: accountName !== null ? "Your account's name" : "",
      });
      parts.push(
        h(
          "p.muted",
          {},
          "Open a room and share its code: friends who join run this game in their browser, in step with yours. " +
            "Two can play at once, on the two controllers; the others watch, and everyone can chat.",
        ),
        s?.note ? h("p.muted.small", {}, s.note) : null,
        h(
          "form.room-open",
          {
            onsubmit: (e: Event) => {
              e.preventDefault();
              const n = name.value.trim();
              if (!n) return name.focus();
              if (accountName === null) saveName(n);
              o.open(n);
            },
          },
          name,
          h("button.primary", { type: "submit" }, "Open a room"),
        ),
      );
    } else if (!s || s.state === "connecting" || (o.guest && s.state === "syncing")) {
      parts.push(h("p", {}, o.guest ? "Joining the room…" : "Opening a room…"));
    } else if (s.state === "ended") {
      parts.push(h("p", {}, s.note ?? "The session ended."), h("a.button.ghost", { href: "#/join" }, "Join another room"));
    }
    if (s && active() && s.code) {
      const link = `${location.origin}${location.pathname}#/join/${s.code}`;
      const copy = h(
        "button.ghost",
        {
          onclick: () => {
            // No clipboard on insecure origins (plain http, but for
            // localhost): show the link, selected, to copy by hand.
            const manual = () => {
              const field = h("input.link-field", { value: link, readOnly: true, "aria-label": "Room link" });
              copy.replaceWith(field);
              field.select();
            };
            if (!navigator.clipboard) return manual();
            navigator.clipboard.writeText(link).then(() => (copy.textContent = "Copied"), manual);
          },
        },
        "Copy link",
      );
      const holders = (port: number) => {
        const names = s.ports[port]!.map((id) => s.members.find((m) => m.id === id)?.name).filter(Boolean);
        return names.length ? names.join(" + ") : "nobody";
      };
      parts.push(
        h("p.room-code", {}, h("span.muted", {}, "Code"), h("strong", {}, formatCode(s.code)), copy),
        h(
          "ul.members",
          {},
          ...s.members.map((m) =>
            h(
              "li",
              {},
              h(
                "span.member-name",
                {},
                h("strong", {}, m.name),
                m.me ? h("span.muted.small", {}, " (you)") : null,
                m.host ? h("span.muted.small", {}, " · host") : null,
              ),
              h(
                "span.muted.small.member-state",
                {},
                !m.playing ? "joining…" : m.rtt !== null ? `${Math.round(m.rtt)} ms` : "",
              ),
              chips(m, s.ports),
            ),
          ),
        ),
        h("p.small", {}, `Controller 1: ${holders(0)} · Controller 2: ${holders(1)}`),
        !o.guest
          ? h("p.muted.small", {}, "Hand out a controller with 🎮 1 or 🎮 2 next to a name; two names on one controller share it.")
          : null,
        s.state === "stalled" && s.waitingFor.length ? h("p.net-stalled", {}, `Waiting for ${s.waitingFor.join(", ")}…`) : null,
        s.delay !== null ? h("p.muted.small", {}, `Your input delay: ${s.delay} frame${s.delay === 1 ? "" : "s"}`) : null,
        s.note ? h("p.muted.small", {}, s.note) : null,
        h(
          "div.net-actions",
          {},
          h("button.ghost", { onclick: () => o.leave() }, o.guest ? "Leave the room" : "Close the room"),
        ),
      );
    }
    body.replaceChildren(...parts.filter((p): p is Node | string => !!p));
    split.hidden = !active();
    if (s) {
      viewSelect.value = s.view ?? "";
      const OPPOSITE: Record<Half, Half> = { top: "bottom", bottom: "top", left: "right", right: "left" };
      viewText.textContent = !s.view
        ? "Everyone sees the whole screen."
        : s.hidden
          ? `You see the ${OPPOSITE[s.hidden]} half; the other one is hidden.`
          : "Each player sees their half; you see everything.";
    }
    chat.hidden = !active() && !log.childElementCount;
  };

  render();
  return {
    element,
    status: (s) => {
      // A new room: a new chat.
      if (s.code && s.code !== net?.code) log.replaceChildren();
      net = s;
      render();
    },
    chat: (line) => {
      const atBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 8;
      log.append(
        line.from === null
          ? h("p.chat-line.notice", {}, h("span.chat-time", {}, time(line.at)), line.text)
          : h(
              "p.chat-line",
              {},
              h("span.chat-time", {}, time(line.at)),
              h("strong", {}, line.name),
              " ",
              line.text,
            ),
      );
      if (atBottom) log.scrollTop = log.scrollHeight;
      chat.hidden = false;
    },
  };
}
