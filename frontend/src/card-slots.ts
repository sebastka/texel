// Choosing the memory cards a game (or a console) starts with: one of the
// user's cards of that console per slot, or a blank card, not kept.

import { api, type Card, type Console } from "./api";
import { h } from "./dom";
import type { Slots } from "./protocol";

/** "12-0": the cards of slots 1 and 2, in an address (0: none). */
export const slotsParam = (s: Slots) => s.map((id) => id ?? 0).join("-");

export function parseSlots(param: string | undefined): Slots | null {
  const m = param?.match(/^(\d+)-(\d+)$/);
  if (!m) return null;
  const id = (v: string) => (Number(v) > 0 ? Number(v) : null);
  return [id(m[1]!), id(m[2]!)];
}

export interface CardSlots {
  element: HTMLElement;
  /** The cards chosen; a new card asked for is created first. */
  value: () => Promise<Slots>;
}

const BLANK = "";
const NEW = "new";

/**
 * Two menus, slots 1 and 2, over the user's cards of `console`.
 * `initial`: the cards to preselect (a game's last ones). For a game
 * (`newName` given), with none of them, slot 1 gets the card used last;
 * without cards at all, a new card named `newName` (created when the game
 * starts). Without `newName`, the menus show `initial` as it is (a console
 * running with those cards).
 */
export function cardSlots(console: Console, initial: Slots, newName: string | null, onChange?: () => void): CardSlots {
  const menus = [0, 1].map((slot) =>
    h("select.card-slot", { "aria-label": `Memory card ${slot + 1}`, onchange: () => onChange?.() }),
  ) as [HTMLSelectElement, HTMLSelectElement];
  const note = h("span.muted.small");
  let cards: Card[] = [];
  const fill = (list: Card[]) => {
    cards = list;
    const known = (id: number | null) => id !== null && list.some((c) => c.id === id);
    let chosen: Slots = [known(initial[0]) ? initial[0] : null, known(initial[1]) ? initial[1] : null];
    if (newName !== null && chosen[0] === null && chosen[1] === null && list.length) {
      // The card used last.
      chosen = [[...list].sort((a, b) => b.updatedAt - a.updatedAt)[0]!.id, null];
    }
    menus.forEach((menu, slot) => {
      const options = list.map((c) => {
        const saves = c.saves?.length ?? 0;
        return h("option", { value: String(c.id), selected: c.id === chosen[slot] }, `${c.name} (${saves} save${saves === 1 ? "" : "s"})`);
      });
      const fresh =
        slot === 0 && !list.length && newName !== null
          ? [h("option", { value: NEW, selected: true }, `New card: ${newName}`)]
          : [];
      menu.replaceChildren(
        ...fresh,
        ...options,
        h("option", { value: BLANK, selected: !fresh.length && chosen[slot] === null }, "Blank card, not kept"),
      );
    });
    note.textContent = list.length
      ? "Saves go to the cards chosen."
      : newName !== null
        ? "You have no memory card for this console yet: one is created for this game."
        : "You have no memory card for this console yet.";
  };
  const loaded = api.cards(console).then(fill, (e: unknown) => {
    note.textContent = `Your memory cards could not be read: ${String(e)}`;
    fill([]);
  });
  const element = h(
    "div.card-slots",
    {},
    h("label", {}, "Memory card 1", menus[0]),
    h("label", {}, "Memory card 2", menus[1]),
    note,
    h("a.small", { href: `#/cards/${console}` }, "Manage memory cards"),
  );
  return {
    element,
    value: async () => {
      await loaded;
      const out: Slots = [null, null];
      for (const slot of [0, 1]) {
        const v = menus[slot]!.value;
        if (v === NEW && newName !== null) {
          const card = await api.createCard(console, newName);
          cards.push(card);
          out[slot] = card.id;
          // Chosen from now on (not created again).
          const option = h("option", { value: String(card.id) }, `${card.name} (0 saves)`);
          menus[slot]!.querySelector(`option[value="${NEW}"]`)?.replaceWith(option);
          menus[slot]!.value = String(card.id);
        } else out[slot] = v ? Number(v) : null;
      }
      return out;
    },
  };
}
