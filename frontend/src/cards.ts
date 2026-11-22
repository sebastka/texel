// The user's memory cards: each game's two cards, the saves on them (with
// their icons), and what can be done with them: delete a save, copy or move
// it to another game's card, download or replace a whole card.

import { ApiError, api, type Card, type CardSave, type Console } from "./api";
import { h } from "./dom";
import type { Shell } from "./main";

const CONSOLE_NAMES: Record<Console, string> = { ps1: "PlayStation", ps2: "PlayStation 2" };

function size(bytes: number): string {
  return bytes >= 1024 * 1024 ? `${(bytes / 1024 / 1024).toFixed(1)} MB` : `${Math.round(bytes / 1024)} KB`;
}

/** A PlayStation save's icon, animated through its frames. */
function icon(save: CardSave): HTMLElement {
  const canvas = h("canvas.save-icon", { width: 16, height: 16 });
  const frames = (save.icon ?? []).map((b64) => {
    const bytes = Uint8ClampedArray.from(atob(b64), (c) => c.charCodeAt(0));
    return new ImageData(bytes, 16, 16);
  });
  const ctx = canvas.getContext("2d");
  if (!ctx || !frames.length) return h("span.save-icon.empty");
  let k = 0;
  ctx.putImageData(frames[0]!, 0, 0);
  if (frames.length > 1) {
    const timer = setInterval(() => {
      if (!canvas.isConnected) return clearInterval(timer);
      k = (k + 1) % frames.length;
      ctx.putImageData(frames[k]!, 0, 0);
    }, 250);
  }
  return canvas;
}

export async function renderCards(shell: Shell) {
  const status = h("p.error", { hidden: true, role: "alert" });
  const fail = (e: unknown) => {
    status.textContent = e instanceof ApiError ? e.message : String(e);
    status.hidden = false;
  };
  const body = h("div.cards", {}, h("p.muted", {}, "Loading…"));
  shell.app.replaceChildren(
    shell.header("cards"),
    h(
      "main.page",
      {},
      h("div.page-head", {}, h("h1", {}, "Memory cards")),
      h(
        "p.muted",
        {},
        "Each game has its own two memory cards, yours alone. Here are the ones your games wrote to, with their saves: " +
          "copy a save to another game's card (games sharing saves, a game's other edition), move it, or delete it.",
      ),
      status,
      body,
    ),
  );
  let cards: Card[];
  try {
    cards = await api.cards();
  } catch (e) {
    body.replaceChildren();
    return fail(e);
  }
  const reload = () => void renderCards(shell);
  const act = async (what: () => Promise<unknown>) => {
    status.hidden = true;
    try {
      await what();
      reload();
    } catch (e) {
      fail(e);
    }
  };
  if (!cards.length) {
    body.replaceChildren(h("p.muted", {}, "No memory card yet: they appear once a game saves."));
    return;
  }
  // Destinations: every game of the console (first discs), each with two
  // slots.
  const destinations = (c: Console) =>
    shell
      .games()
      .filter((g) => g.console === c && (!g.discs.length || g.discs[0]?.id === g.id))
      .sort((a, b) => a.title.localeCompare(b.title));

  const copyForm = (card: Card, save: CardSave) => {
    const game = card.game!;
    const target = h(
      "select",
      { "aria-label": "Destination" },
      ...destinations(card.console).flatMap((g) =>
        [1, 2]
          .filter((slot) => !(g.id === game.id && slot === card.slot))
          .map((slot) => h("option", { value: `${g.id}/${slot}` }, `${g.title} — card ${slot}`)),
      ),
    );
    const go = (move: boolean) => () => {
      const [toGame, toSlot] = target.value.split("/");
      void act(() => api.copySave(game.id, card.slot, save.id, toGame!, Number(toSlot), move));
    };
    return h(
      "div.copy-form",
      { hidden: true },
      target,
      h("button.ghost.small-button", { onclick: go(false) }, "Copy"),
      h("button.ghost.small-button", { onclick: go(true) }, "Move"),
    );
  };

  const saveRow = (card: Card, save: CardSave) => {
    const form = card.game ? copyForm(card, save) : null;
    const del = h(
      "button.ghost.small-button.danger",
      {
        onclick: () => {
          if (del.dataset.armed) {
            void act(() => api.deleteSave(card.game!.id, card.slot, save.id));
          } else {
            // Asked twice: no dialog boxes.
            del.dataset.armed = "1";
            del.textContent = "Delete: sure?";
          }
        },
      },
      "Delete",
    );
    return h(
      "li.save",
      {},
      icon(save),
      h("div.save-name", {}, h("div", {}, save.title || save.id), h("div.muted.small.mono", {}, save.id)),
      h("span.muted.small.nowrap", {}, size(save.size)),
      card.game
        ? h(
            "div.save-actions",
            {},
            h(
              "button.ghost.small-button",
              {
                onclick: () => {
                  if (form) form.hidden = !form.hidden;
                },
              },
              "Copy or move…",
            ),
            del,
          )
        : null,
      form,
    );
  };

  const cardBox = (card: Card) => {
    const g = card.game;
    const upload = h("input", {
      type: "file",
      hidden: true,
      onchange: () => {
        const file = upload.files?.[0];
        if (!file || !g) return;
        void act(async () => api.putCard(g.id, card.slot, await file.arrayBuffer()));
      },
    }) as HTMLInputElement;
    const remove = h(
      "button.ghost.small-button.danger",
      {
        onclick: () => {
          if (!g) return;
          if (remove.dataset.armed) void act(() => api.deleteCard(g.id, card.slot));
          else {
            remove.dataset.armed = "1";
            remove.textContent = "Delete the card: sure?";
          }
        },
      },
      "Delete the card",
    );
    const saves = card.saves ?? [];
    return h(
      "section.card-box",
      {},
      h(
        "div.card-head",
        {},
        h(
          "div",
          {},
          h("h2", {}, g ? h("a", { href: `#/game/${g.id}` }, g.title) : card.key, ` · card ${card.slot}`),
          h(
            "div.muted.small",
            {},
            g?.serial ?? card.key,
            card.error
              ? ` · ${card.error}`
              : card.formatted === false
                ? " · not formatted"
                : ` · ${saves.length} save${saves.length === 1 ? "" : "s"}, ${size(card.free ?? 0)} free of ${size(card.capacity ?? 0)}`,
          ),
        ),
        g
          ? h(
              "div.card-actions",
              {},
              h(
                "a.button.ghost.small-button",
                { href: api.cardUrl(g.id, card.slot), download: `${g.serial ?? g.id}-${card.slot}.${card.console === "ps1" ? "mcd" : "ps2"}` },
                "Download",
              ),
              h("button.ghost.small-button", { onclick: () => upload.click(), title: "Replace this card with an image file" }, "Replace…"),
              remove,
              upload,
            )
          : h("span.muted.small", {}, "This game left the library."),
      ),
      saves.length ? h("ul.saves", {}, ...saves.map((s) => saveRow(card, s))) : null,
    );
  };

  const sections = (["ps1", "ps2"] as Console[])
    .map((c) => [c, cards.filter((card) => card.console === c)] as const)
    .filter(([, list]) => list.length)
    .map(([c, list]) => h("section", {}, h("h2.console-title", {}, CONSOLE_NAMES[c]), ...list.map(cardBox)));
  body.replaceChildren(...sections);
}
