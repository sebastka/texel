// Administration: invitations (tokens handed over to friends, who type
// them when registering), users (reset tokens, deletion), and features
// still being tried out (the server's settings).

import { avatar } from "./account-menu";
import { ApiError, api, type InviteRow, type Settings, type UserRow } from "./api";
import { h } from "./dom";
import type { Shell } from "./main";

const when = (secs: number | null) => (secs ? new Date(secs * 1000).toLocaleString() : "—");

/** A token just created: shown once, with its link and copy buttons. */
function tokenBox(token: string, what: string): HTMLElement {
  const link = `${location.origin}/register?token=${encodeURIComponent(token)}`;
  const copy = (text: string, button: HTMLButtonElement) => () =>
    void navigator.clipboard?.writeText(text).then(() => {
      button.textContent = "Copied";
    });
  const copyToken = h("button.ghost.small-button", {}, "Copy the token") as HTMLButtonElement;
  const copyLink = h("button.ghost.small-button", {}, "Copy the link") as HTMLButtonElement;
  copyToken.onclick = copy(token, copyToken);
  copyLink.onclick = copy(link, copyLink);
  return h(
    "div.token-box",
    { role: "status" },
    h("p", {}, `${what}: hand it over (it is shown only now).`),
    h("p.token.mono", {}, token),
    h("p.small.mono.token-link", {}, link),
    h("div.card-actions", {}, copyToken, copyLink),
  );
}

export async function renderAdmin(shell: Shell) {
  const me = shell.me();
  if (me?.role !== "admin") {
    shell.app.replaceChildren(shell.header("admin"), h("main.page", {}, h("p.error", {}, "For administrators.")));
    return;
  }
  const error = h("p.error", { hidden: true, role: "alert" });
  const fail = (e: unknown) => {
    error.textContent = e instanceof ApiError ? e.message : String(e);
    error.hidden = false;
  };
  const fresh = h("div");
  const invitesBody = h("tbody");
  const usersBody = h("tbody");
  const note = h("input", { placeholder: "Who it is for (a note)", maxLength: 100, "aria-label": "Note" });
  const features = h("div.admin-features");

  /** A switch per setting, saved as soon as it changes. */
  const renderFeatures = (settings: Settings) => {
    const toggle = (key: "netplayRollback" | "netplayDatagrams", title: string, about: string, warning?: string | null) => {
      const box = h("input.switch", { type: "checkbox", checked: settings[key], "aria-label": title }) as HTMLInputElement;
      box.onchange = () => {
        box.disabled = true;
        void api
          .saveSettings({ ...settings, [key]: box.checked })
          .then(renderFeatures, (e: unknown) => {
            box.checked = settings[key];
            fail(e);
          })
          .finally(() => (box.disabled = false));
      };
      return h(
        "label.admin-feature",
        {},
        h(
          "span.admin-feature-text",
          {},
          h("strong", {}, title),
          h("span.muted.small", {}, about),
          warning ? h("span.small.error", {}, warning) : null,
        ),
        box,
      );
    };
    features.replaceChildren(
      toggle(
        "netplayRollback",
        "Netplay: rollback",
        "Rooms opened from now on run ahead with the other players' last inputs, and go back a few frames when one " +
          "turns out otherwise: the input delay is hidden, at the cost of brief corrections and of more work for " +
          "each player's machine. Off: each frame waits for every player's input (lockstep). Rooms already open keep " +
          "theirs.",
      ),
      toggle(
        "netplayDatagrams",
        "Netplay: inputs as datagrams",
        "Players' inputs also go over WebTransport (UDP), each carrying the last few: a lost packet no longer holds " +
          "the game back while it is sent again. Needs the server to listen for it (TEXEL_WEBTRANSPORT_LISTEN, with " +
          "a certificate), and a browser with WebTransport; the others play as before. Players joining from now on.",
        settings.datagramsListening ? null : "The server does not listen for them: on, this changes nothing.",
      ),
    );
  };

  const confirmButton = (label: string, run: () => void) => {
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
  };

  const inviteRow = (i: InviteRow) => {
    const now = Date.now() / 1000;
    const state = i.usedAt
      ? `Used ${when(i.usedAt)}${i.usedBy ? ` by ${i.usedBy}` : ""}`
      : i.expiresAt < now
        ? "Expired"
        : `Valid until ${when(i.expiresAt)}`;
    return h(
      "tr",
      {},
      h("td", { "data-label": "Kind" }, i.kind === "reset" ? `Reset (${i.user ?? "?"})` : "Invitation"),
      h("td", { "data-label": "Note" }, i.note || "—"),
      h("td", { "data-label": "Created" }, `${when(i.createdAt)}${i.createdBy ? ` by ${i.createdBy}` : ""}`),
      h("td", { "data-label": "State" }, state),
      h(
        "td",
        {},
        confirmButton("Delete", () => void api.deleteInvite(i.id).then(refresh, fail)),
      ),
    );
  };

  const userRow = (u: UserRow) =>
    h(
      "tr",
      {},
      h(
        "td",
        { "data-label": "Name" },
        h("span.user-cell", {}, avatar(u), u.name, u.role === "admin" ? h("span.badge.accent", {}, "Admin") : null),
      ),
      h("td", { "data-label": "Signs in with" }, u.kind === "oidc" ? "Sign-in provider" : (u.email ?? "—")),
      h("td", { "data-label": "Since" }, when(u.createdAt)),
      h("td", { "data-label": "Last seen" }, when(u.lastSeenAt)),
      h(
        "td",
        {},
        u.kind === "password"
          ? h(
              "button.ghost.small-button",
              {
                title: "A token with which they set a new password (on the registration page)",
                onclick: () =>
                  void api.resetUser(u.id).then((t) => {
                    fresh.replaceChildren(tokenBox(t.token, `A reset token for ${u.name}`));
                    void refresh();
                  }, fail),
              },
              "Reset password",
            )
          : null,
        u.id !== me.id ? confirmButton("Delete", () => void api.deleteUser(u.id).then(refresh, fail)) : null,
      ),
    );

  async function refresh() {
    try {
      const [invites, users, settings] = await Promise.all([api.invites(), api.users(), api.settings()]);
      invitesBody.replaceChildren(...invites.map(inviteRow));
      usersBody.replaceChildren(...users.map(userRow));
      renderFeatures(settings);
    } catch (e) {
      fail(e);
    }
  }

  const table = (heads: string[], body: HTMLElement) =>
    h("div.table-wrap", {}, h("table.admin-table", {}, h("thead", {}, h("tr", {}, ...heads.map((t) => h("th", {}, t)))), body));

  shell.app.replaceChildren(
    shell.header("admin"),
    h(
      "main.page",
      {},
      h("div.page-head", {}, h("h1", {}, "Administration")),
      error,
      h("h2", {}, "Invitations"),
      h(
        "p.muted",
        {},
        "Invite a friend without an account at your sign-in provider: they register with the token, choosing their " +
          "e-mail address, name and password. A token is used once.",
      ),
      h(
        "form.invite-form",
        {
          onsubmit: (e: Event) => {
            e.preventDefault();
            void api.createInvite(note.value).then((t) => {
              fresh.replaceChildren(tokenBox(t.token, "An invitation"));
              note.value = "";
              void refresh();
            }, fail);
          },
        },
        note,
        h("button.primary", { type: "submit" }, "Create an invitation"),
      ),
      fresh,
      table(["Kind", "Note", "Created", "State", ""], invitesBody),
      h("h2", {}, "Users"),
      h(
        "p.muted",
        {},
        "Administrators are the sign-in provider's users the configuration names; everyone else is a guest. " +
          "Deleting a user deletes their memory cards and save states.",
      ),
      table(["Name", "Signs in with", "Since", "Last seen", ""], usersBody),
      h("h2", {}, "Features"),
      h("p.muted", {}, "Features still being tried out: off until turned on here, for everyone."),
      features,
    ),
  );
  await refresh();
}
