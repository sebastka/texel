// The netplay room's bar, under the header on every page while in a room:
// its code, its members and who has which controller, what it plays, and
// the chat: folded away, or beside the screen in the room's game.

import { h, type Child } from "./dom";
import type { Room } from "./room-link";
import type { ChatLine, Half, NetStatus, Ports, SplitView } from "./protocol";

const NAME_KEY = "texel.name";

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

export interface BarActions {
  setPorts: (ports: Ports) => void;
  setView: (view: SplitView) => void;
  say: (text: string) => void;
  leave: () => void;
  /** Hides the bar of a room that ended. */
  dismiss: () => void;
}

/** What the bar shows. */
export type BarState =
  | {
      room: Room;
      /** The room's game, as this member's player runs it (none while
       * this member is not in it). */
      net: NetStatus | null;
      /** The room's game's title. */
      title: string | null;
      /** This page shows the room's game. */
      here: boolean;
    }
  | { ended: string };

export interface RoomBar {
  element: HTMLElement;
  render: (s: BarState | null) => void;
  chat: (line: ChatLine) => void;
  /** Shows the chat (and the split screen) in `place`, beside the game;
   * null: back in the bar, folded. */
  dock: (place: HTMLElement | null) => void;
}

const time = (ms: number) => {
  const d = new Date(ms);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
};

const OPPOSITE: Record<Half, Half> = { top: "bottom", bottom: "top", left: "right", right: "left" };

export function roomBar(a: BarActions): RoomBar {
  const body = h("div.room-line");
  const members = h("ul.room-members");
  const log = h("div.chat-log", { role: "log", "aria-live": "polite" });
  const message = h("input.chat-input", { placeholder: "Message", maxLength: 500, "aria-label": "Chat message" });
  let unread = 0;
  let current: BarState | null = null;
  const chatButton = h(
    "button.ghost.small-button.toggle",
    {
      "aria-expanded": "false",
      onclick: () => {
        more.hidden = !more.hidden;
        chatButton.setAttribute("aria-expanded", String(!more.hidden));
        if (!more.hidden) {
          unread = 0;
          log.scrollTop = log.scrollHeight;
          message.focus();
        }
        render(current);
      },
    },
    "Chat",
  ) as HTMLButtonElement;
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
      title:
        "For split-screen games: each player's browser hides the other player's half. Those who watch, or hold both " +
        "controllers, see everything.",
      onchange: (e: Event) => {
        const v = (e.target as HTMLSelectElement).value;
        a.setView(v ? (v as Half) : null);
      },
    },
    ...VIEWS.map(([v, label]) => h("option", { value: v ?? "" }, label)),
  );
  const viewText = h("span.small");
  const split = h("div.room-split", {}, h("span.muted.small", {}, "Split screen:"), viewSelect, viewText);
  // The chat and the split screen: in the bar (`more`), or docked.
  const chatBox = h(
    "div.room-chat",
    {},
    log,
    h(
      "form.chat-form",
      {
        onsubmit: (e: Event) => {
          e.preventDefault();
          const text = message.value.trim();
          if (!text) return;
          a.say(text);
          message.value = "";
        },
      },
      message,
      h("button.ghost", { type: "submit" }, "Send"),
    ),
    split,
  );
  const more = h("div.room-more", { hidden: true }, chatBox);
  let docked = false;
  const element = h("section.room-bar", { hidden: true, "aria-label": "Room" }, body, members, more);

  const chips = (room: Room, id: number, ports: Ports, net: NetStatus | null) =>
    h(
      "span.chips",
      {},
      ...[0, 1].map((port) => {
        const on = ports[port]!.includes(id);
        const label = `Controller ${port + 1}`;
        if (!room.isHost) return on ? h("span.chip.on", { title: label }, `🎮 ${port + 1}`) : null;
        // In a game, only to those running it (the others would hold it up).
        const playing = !net || net.members.some((m) => m.id === id && m.playing);
        return h(
          `button.chip${on ? ".on" : ""}`,
          {
            title: playing ? `${on ? "Take back" : "Give"} ${label.toLowerCase()}` : "Not in the game yet",
            disabled: !playing,
            "aria-pressed": String(on),
            onclick: () => {
              const next = ports.map((ids, i) =>
                i === port ? (on ? ids.filter((x) => x !== id) : [...ids, id]) : [...ids],
              ) as Ports;
              a.setPorts(next);
            },
          },
          `🎮 ${port + 1}`,
        );
      }),
    );

  function render(s: BarState | null) {
    current = s;
    element.hidden = !s;
    if (!s) return;
    if ("ended" in s) {
      body.replaceChildren(
        h("span.room-label", {}, "Room"),
        h("span.room-state", {}, s.ended),
        h("span.room-actions", {}, h("button.ghost.small-button", { onclick: () => a.dismiss() }, "Dismiss")),
      );
      members.replaceChildren();
      more.hidden = true;
      return;
    }
    const { room, net, title, here } = s;
    if (room.state === "connecting" || !room.code) {
      body.replaceChildren(h("span.room-label", {}, "Room"), h("span.room-state", {}, "Connecting…"));
      members.replaceChildren();
      return;
    }
    const link = `${location.origin}${location.pathname}#/join/${room.code}`;
    const copy = h(
      "button.ghost.small-button",
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
    // What the room does, from this member's side.
    let doing: Child;
    if (!room.game) {
      doing = room.isHost
        ? h("span", {}, "Start a game from the library: everyone plays it with you.")
        : h("span", {}, `${room.hostName} is choosing a game.`);
    } else if (!here) {
      doing = h("span", {}, `Playing ${title ?? "a game"}. `, h("a", { href: "#/room" }, "Back to the game"));
    } else if (!net || net.state === "syncing") {
      doing = h("span", {}, `Getting ${room.hostName}'s game…`);
    } else if (net.state === "stalled" && net.waitingFor.length) {
      doing = h("span.net-stalled", {}, `Waiting for ${net.waitingFor.join(", ")}…`);
    } else {
      doing = h(
        "span",
        {},
        `Playing ${title ?? "a game"}`,
        net.delay !== null ? h("span.muted.small", {}, ` · input delay ${net.delay}`) : null,
        room.datagrams === "on"
          ? h("span.muted.small", { title: "Inputs also go as datagrams: a lost packet no longer holds the game back" }, " · datagrams")
          : room.datagrams === "failed"
            ? h("span.muted.small", { title: "The datagram session could not open (UDP blocked?): inputs go over the WebSocket only" }, " · no datagrams")
            : null,
        net.rollback
          ? h(
              "span.muted.small",
              { title: "Frames run on predicted inputs now, and frames run again over the last two seconds (corrections)" },
              ` · rollback: ${net.rollback.ahead} ahead, ${net.rollback.replayed} replayed`,
            )
          : null,
      );
    }
    const heir = room.heir;
    const leaveTitle = !room.isHost
      ? ""
      : heir
        ? `${heir.name} becomes the host and the room goes on.`
        : "Nobody else here has an account to take over: the room closes.";
    chatButton.textContent = unread && more.hidden ? `Chat (${unread})` : "Chat";
    chatButton.hidden = docked;
    body.replaceChildren(
      h("span.room-label", {}, "Room"),
      h("strong.room-code", {}, formatCode(room.code)),
      copy,
      h("span.room-state", {}, doing),
      ...(net?.note ? [h("span.muted.small.room-note", {}, net.note)] : []),
      h(
        "span.room-actions",
        {},
        chatButton,
        h(
          "button.ghost.small-button",
          { title: leaveTitle, onclick: () => a.leave() },
          room.isHost && !heir ? "Close the room" : "Leave the room",
        ),
      ),
    );
    const inGame = net && here ? net : null;
    const ports = inGame ? inGame.ports : room.ports;
    members.replaceChildren(
      ...[...room.members.values()].map((m) => {
        const status = inGame?.members.find((x) => x.id === m.id);
        return h(
          "li",
          {},
          h(
            "span.member-name",
            {},
            h("strong", {}, m.name),
            m.id === room.me ? h("span.muted.small", {}, " (you)") : null,
            m.id === room.host ? h("span.muted.small", {}, " · host") : null,
          ),
          h(
            "span.muted.small.member-state",
            {},
            status && !status.playing ? "not in the game" : m.rtt !== null ? `${Math.round(m.rtt)} ms` : "",
          ),
          chips(room, m.id, ports, inGame),
        );
      }),
    );
    const view = inGame?.view ?? room.view;
    viewSelect.value = view ?? "";
    viewSelect.hidden = !room.isHost;
    viewText.hidden = room.isHost;
    viewText.textContent = !view
      ? "everyone sees the whole screen."
      : inGame?.hidden
        ? `you see the ${OPPOSITE[inGame.hidden]} half; the other one is hidden.`
        : "each player sees their half; you see everything.";
  }

  return {
    element,
    render,
    chat: (line) => {
      const atBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 8;
      log.append(
        line.from === null
          ? h("p.chat-line.notice", {}, h("span.chat-time", {}, time(line.at)), line.text)
          : h("p.chat-line", {}, h("span.chat-time", {}, time(line.at)), h("strong", {}, line.name), " ", line.text),
      );
      if (atBottom) log.scrollTop = log.scrollHeight;
      if (more.hidden && !docked && line.from !== null) {
        unread++;
        render(current);
      }
    },
    dock: (place) => {
      if (place) {
        if (chatBox.parentElement !== place) place.append(chatBox);
        docked = true;
        more.hidden = true;
        chatButton.setAttribute("aria-expanded", "false");
        unread = 0;
        log.scrollTop = log.scrollHeight;
      } else if (docked) {
        more.append(chatBox);
        docked = false;
      }
      render(current);
    },
  };
}
