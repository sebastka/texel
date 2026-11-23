// The user's memory cards, per console: their own, named, inserted in
// whichever game they play (chosen before it starts). Here: creating,
// renaming and deleting cards, and their saves (with their icons): delete
// a save, copy or move it to another card, download or replace a card.

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

/** A button that acts on its second click: no dialog boxes. */
function confirmButton(label: string, run: () => void): HTMLElement {
  const b = h(
    "button.ghost.small-button.danger",
    {
      onclick: () => {
        if (b.dataset.armed) run();
        else {
          b.dataset.armed = "1";
          b.textContent = `${label}: sure?`;
        }
      },
    },
    label,
  );
  return b;
}

export async function renderCards(shell: Shell, platform: Console) {
  const status = h("p.error", { hidden: true, role: "alert" });
  const fail = (e: unknown) => {
    status.textContent = e instanceof ApiError ? e.message : String(e);
    status.hidden = false;
  };
  const body = h("div.cards", {}, h("p.muted", {}, "Loading…"));
  const tabs = h("div");
  const name = h("input", { placeholder: "Name", maxLength: 64, required: true, "aria-label": "New card's name" });
  const reload = () => void renderCards(shell, platform);
  const act = async (what: () => Promise<unknown>) => {
    status.hidden = true;
    try {
      await what();
      reload();
    } catch (e) {
      fail(e);
    }
  };
  const create = h(
    "form.card-new",
    {
      onsubmit: (e: Event) => {
        e.preventDefault();
        const n = name.value.trim();
        if (n) void act(() => api.createCard(platform, n));
      },
    },
    name,
    h("button.primary", { type: "submit" }, "New card"),
  );
  shell.app.replaceChildren(
    shell.header("cards"),
    h(
      "main.page",
      {},
      h("div.page-head", {}, h("h1", {}, "Memory cards")),
      tabs,
      h(
        "p.muted",
        {},
        "Your memory cards, yours alone: you choose which ones a game gets before it starts, and it saves to them. " +
          "Here, their saves: copy one to another card, move it, or delete it.",
      ),
      create,
      status,
      body,
    ),
  );
  let all: Card[];
  try {
    all = await api.cards();
  } catch (e) {
    body.replaceChildren();
    return fail(e);
  }
  tabs.replaceChildren(shell.consoleTabs("cards", platform, (c) => all.filter((card) => card.console === c).length));
  const cards = all.filter((c) => c.console === platform);
  name.value = `Card ${cards.length + 1}`;
  if (!cards.length) {
    body.replaceChildren(
      h(
        "p.muted",
        {},
        `No ${CONSOLE_NAMES[platform]} memory card yet: create one here, or one is made for the first game you start.`,
      ),
    );
    return;
  }

  const copyForm = (card: Card, save: CardSave) => {
    const others = cards.filter((c) => c.id !== card.id);
    const target = h(
      "select",
      { "aria-label": "Destination card" },
      ...others.map((c) => h("option", { value: String(c.id) }, c.name)),
    );
    const go = (move: boolean) => () => void act(() => api.copySave(card.id, save.id, Number(target.value), move));
    return h(
      "div.copy-form",
      { hidden: true },
      ...(others.length
        ? [
            target,
            h("button.ghost.small-button", { onclick: go(false) }, "Copy"),
            h("button.ghost.small-button", { onclick: go(true) }, "Move"),
          ]
        : [h("span.muted.small", {}, "Create another card to copy it to.")]),
    );
  };

  const saveRow = (card: Card, save: CardSave) => {
    const form = copyForm(card, save);
    return h(
      "li.save",
      {},
      icon(save),
      h("div.save-name", {}, h("div", {}, save.title || save.id), h("div.muted.small.mono", {}, save.id)),
      h("span.muted.small.nowrap", {}, size(save.size)),
      h(
        "div.save-actions",
        {},
        h("button.ghost.small-button", { onclick: () => (form.hidden = !form.hidden) }, "Copy or move…"),
        confirmButton("Delete", () => void act(() => api.deleteSave(card.id, save.id))),
      ),
      form,
    );
  };

  const cardBox = (card: Card) => {
    const upload = h("input", {
      type: "file",
      hidden: true,
      onchange: () => {
        const file = upload.files?.[0];
        if (file) void act(async () => api.putCard(card.id, await file.arrayBuffer()));
      },
    }) as HTMLInputElement;
    // Renaming in place.
    const title = h("h2", {}, card.name);
    const rename = h("input", { value: card.name, maxLength: 64, required: true, "aria-label": "Card name" });
    const renameForm = h(
      "form.card-rename",
      {
        hidden: true,
        onsubmit: (e: Event) => {
          e.preventDefault();
          const n = rename.value.trim();
          if (n && n !== card.name) void act(() => api.renameCard(card.id, n));
          else renameForm.hidden = true;
        },
      },
      rename,
      h("button.ghost.small-button", { type: "submit" }, "Save"),
    );
    const saves = card.saves ?? [];
    const updated = new Date(card.updatedAt * 1000).toLocaleString();
    return h(
      "section.card-box",
      {},
      h(
        "div.card-head",
        {},
        h(
          "div",
          {},
          title,
          renameForm,
          h(
            "div.muted.small",
            {},
            card.error
              ? card.error
              : card.formatted === false
                ? "Not formatted: a game formats it"
                : `${saves.length} save${saves.length === 1 ? "" : "s"}, ${size(card.free ?? 0)} free of ${size(card.capacity ?? 0)}`,
            ` · written ${updated}`,
          ),
        ),
        h(
          "div.card-actions",
          {},
          h(
            "button.ghost.small-button",
            {
              onclick: () => {
                renameForm.hidden = !renameForm.hidden;
                if (!renameForm.hidden) rename.select();
              },
            },
            "Rename",
          ),
          h(
            "a.button.ghost.small-button",
            { href: api.cardUrl(card.id), download: `${card.name}.${card.console === "ps1" ? "mcd" : "ps2"}` },
            "Download",
          ),
          h("button.ghost.small-button", { onclick: () => upload.click(), title: "Replace this card with an image file" }, "Replace…"),
          confirmButton("Delete the card", () => void act(() => api.deleteCard(card.id))),
          upload,
        ),
      ),
      saves.length ? h("ul.saves", {}, ...saves.map((s) => saveRow(card, s))) : null,
    );
  };

  body.replaceChildren(...cards.map(cardBox));
}
