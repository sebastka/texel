// The header's avatar: the user's initials on a colour of their own, and,
// on click, a menu with their account and signing out.

import type { User } from "./api";
import { h } from "./dom";

/** Colours of avatars: `.avatar-c0` to `.avatar-c7` in style.css. */
const COLOURS = 8;

/** One or two letters: the first letters of the name's first and last
 * words ("Ada Lovelace" → "AL", "Ada" → "A"); letters, not code units, so
 * accented and non-Latin names work. An e-mail address's domain says
 * nothing about the person: only its local part counts. */
export function initials(name: string): string {
  const letter = /\p{L}|\p{N}/u;
  const words = name
    .split("@")[0]!
    .split(/[\s._-]+/u)
    .filter((w) => letter.test(w));
  const first = (w: string | undefined) => (w ? (Array.from(w).find((c) => letter.test(c)) ?? "") : "");
  const head = first(words[0]);
  const tail = words.length > 1 ? first(words[words.length - 1]) : "";
  return (head + tail).toLocaleUpperCase() || "?";
}

/** A user's colour: a hash (FNV-1a) of their id, the same everywhere and
 * whatever their name becomes. */
export function avatarColour(user: Pick<User, "id">): number {
  let h = 0x811c9dc5;
  for (const c of `user-${user.id}`) {
    h ^= c.codePointAt(0) ?? 0;
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h % COLOURS;
}

/** The user's picture, else (or if it fails to load) their initials. */
export function avatar(user: Pick<User, "id" | "name" | "avatar">, size: "small" | "large" = "small"): HTMLElement {
  const letters = () =>
    h(
      `span.avatar.avatar-${size}.avatar-c${avatarColour(user)}`,
      { role: "img", "aria-label": user.name },
      h("span", { "aria-hidden": "true" }, initials(user.name)),
    );
  if (!user.avatar) return letters();
  const img = h(`img.avatar.avatar-${size}.avatar-picture`, { src: user.avatar, alt: user.name, decoding: "async" });
  img.addEventListener("error", () => img.replaceWith(letters()), { once: true });
  return img;
}

let menus = 0;

/** The avatar button and its menu. */
export function accountMenu(user: User, signOut: () => void): HTMLElement {
  const id = `account-menu-${++menus}`;
  const button = h(
    "button.avatar-button",
    {
      type: "button",
      "aria-haspopup": "menu",
      "aria-expanded": "false",
      "aria-label": `Account: ${user.name}`,
      title: user.name,
    },
    avatar(user),
  ) as HTMLButtonElement;
  const entry = (label: string, run: () => void) =>
    h(
      "button.menu-entry",
      {
        type: "button",
        role: "menuitem",
        tabIndex: -1,
        onclick: () => {
          close(false);
          run();
        },
      },
      label,
    );
  const detail = user.email ?? (user.oidc ? "Single sign-on" : null);
  const menu = h(
    "div.account-menu",
    { id, role: "menu", "aria-label": "Account", hidden: true },
    h(
      "div.account-head",
      { role: "none" },
      h("strong", {}, user.name),
      detail ? h("span.muted.small", {}, detail) : null,
      user.role === "admin" ? h("span.badge.accent", {}, "Administrator") : null,
    ),
    entry("Your account", () => (location.hash = "#/account")),
    entry("Memory cards", () => (location.hash = "#/cards")),
    entry("Sign out", signOut),
  );
  const root = h("div.account", {}, button, menu);
  const items = () => Array.from(menu.querySelectorAll<HTMLElement>('[role="menuitem"]'));

  // Closes on a click or focus outside it.
  const outside = (e: Event) => {
    if (!root.contains(e.target as Node)) close(false);
  };
  function open() {
    menu.hidden = false;
    button.setAttribute("aria-expanded", "true");
    button.setAttribute("aria-controls", id);
    items()[0]?.focus();
    document.addEventListener("pointerdown", outside);
    document.addEventListener("focusin", outside);
  }
  function close(focusButton: boolean) {
    if (menu.hidden) return;
    menu.hidden = true;
    button.setAttribute("aria-expanded", "false");
    button.removeAttribute("aria-controls");
    document.removeEventListener("pointerdown", outside);
    document.removeEventListener("focusin", outside);
    if (focusButton) button.focus();
  }

  button.addEventListener("click", () => (menu.hidden ? open() : close(false)));
  button.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      open();
    }
  });
  menu.addEventListener("keydown", (e) => {
    const list = items();
    const i = list.indexOf(document.activeElement as HTMLElement);
    const focus = (n: number) => list[(n + list.length) % list.length]?.focus();
    switch (e.key) {
      case "ArrowDown":
        focus(i + 1);
        break;
      case "ArrowUp":
        focus(i - 1);
        break;
      case "Home":
        focus(0);
        break;
      case "End":
        focus(list.length - 1);
        break;
      case "Escape":
        close(true);
        break;
      case "Tab":
        close(false);
        return;
      default:
        return;
    }
    e.preventDefault();
  });
  return root;
}
