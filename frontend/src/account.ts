// Signing in (with the OpenID Connect provider, or an e-mail address and a
// password), registering with an invitation token (or setting a new
// password with a reset token), and one's own account.

import { ApiError, api } from "./api";
import { h } from "./dom";
import type { Shell } from "./main";

const MIN_PASSWORD = 10;

function message(e: unknown): string {
  return e instanceof ApiError ? e.message : String(e);
}

function errorLine() {
  const p = h("p.error", { hidden: true, role: "alert" });
  return {
    el: p,
    show: (text: string) => {
      p.textContent = text;
      p.hidden = false;
    },
    hide: () => (p.hidden = true),
  };
}

export function renderSignIn(shell: Shell, failure?: string) {
  const error = errorLine();
  if (failure) error.show(failure);
  const email = h("input", { type: "email", autocomplete: "username", required: true, "aria-label": "E-mail address" });
  const password = h("input", {
    type: "password",
    autocomplete: "current-password",
    required: true,
    "aria-label": "Password",
  });
  const submit = h("button.primary", { type: "submit" }, "Sign in");
  const form = h(
    "form.auth-form",
    {
      onsubmit: async (e: Event) => {
        e.preventDefault();
        error.hide();
        submit.disabled = true;
        try {
          shell.signedIn(await api.login(email.value, password.value));
        } catch (err) {
          error.show(message(err));
          password.select();
        } finally {
          submit.disabled = false;
        }
      },
    },
    h("label", {}, "E-mail address", email),
    h("label", {}, "Password", password),
    submit,
  );
  const provider = h("div.auth-provider");
  shell.app.replaceChildren(
    shell.header(null),
    h(
      "main.auth",
      {},
      h("h1", {}, "Sign in"),
      h("p.muted", {}, "Texel's games are for its members. Friends you play with can also join a room with its code, without an account (Join)."),
      provider,
      error.el,
      form,
      h("p.muted.small", {}, "Invited? ", h("a", { href: "/register" }, "Register with your invitation"), "."),
    ),
  );
  void api.authConfig().then(
    (c) => {
      if (!c.oidc) return;
      // Back to the page asked for, after the provider.
      const back = location.hash.startsWith("#/") && !location.hash.startsWith("#/signin") ? location.hash : "";
      provider.replaceChildren(
        h(
          "a.button.primary.wide-button",
          { href: `/auth/oidc/login?return=${encodeURIComponent(back)}` },
          `Sign in with ${c.oidc}`,
        ),
        h("p.muted.small.auth-or", {}, "or, invited, with your e-mail address:"),
      );
    },
    () => undefined,
  );
  if (!failure) email.focus();
}

export function renderRegister(shell: Shell, initialToken: string) {
  const error = errorLine();
  const token = h("input.token-input", {
    value: initialToken,
    placeholder: "ABCDE-FGHJK-MNPQR-STUVW",
    autocomplete: "off",
    spellcheck: "false",
    required: true,
    "aria-label": "Invitation token",
  });
  const about = h("p.muted.small", { hidden: true });
  const email = h("input", { type: "email", autocomplete: "email", "aria-label": "E-mail address" });
  const name = h("input", { maxLength: 64, autocomplete: "nickname", "aria-label": "Name" });
  const password = h("input", {
    type: "password",
    autocomplete: "new-password",
    minLength: MIN_PASSWORD,
    required: true,
    "aria-label": "Password",
  });
  const again = h("input", { type: "password", autocomplete: "new-password", required: true, "aria-label": "Password again" });
  const emailRow = h("label", {}, "E-mail address (you sign in with it)", email);
  const nameRow = h("label", {}, "Name (others see it)", name);
  const submit = h("button.primary", { type: "submit" }, "Register");
  let kind: "invite" | "reset" | null = null;

  // What the token is for: an invitation (address, name, password) or a
  // reset (a new password for an account).
  const lookUp = async () => {
    const t = token.value.trim();
    kind = null;
    about.hidden = true;
    if (!t) return;
    try {
      const i = await api.invite(t);
      kind = i.kind;
      error.hide();
      const reset = i.kind === "reset";
      emailRow.hidden = reset;
      nameRow.hidden = reset;
      email.required = !reset;
      name.required = !reset;
      submit.textContent = reset ? "Set the new password" : "Register";
      about.textContent = reset
        ? `A new password for ${i.email ?? "your account"}.`
        : i.note
          ? `An invitation (${i.note}).`
          : "An invitation.";
      about.hidden = false;
    } catch (e) {
      error.show(message(e));
    }
  };
  token.addEventListener("change", () => void lookUp());

  const form = h(
    "form.auth-form",
    {
      onsubmit: async (e: Event) => {
        e.preventDefault();
        error.hide();
        if (password.value !== again.value) return error.show("The two passwords differ.");
        if (password.value.length < MIN_PASSWORD) return error.show(`A password has at least ${MIN_PASSWORD} characters.`);
        if (!kind) await lookUp();
        if (!kind) return;
        submit.disabled = true;
        try {
          const user = await api.register({
            token: token.value,
            email: kind === "invite" ? email.value : undefined,
            name: kind === "invite" ? name.value : undefined,
            password: password.value,
          });
          shell.signedIn(user);
        } catch (err) {
          error.show(message(err));
        } finally {
          submit.disabled = false;
        }
      },
    },
    h("label", {}, "Invitation token", token),
    about,
    emailRow,
    nameRow,
    h("label", {}, `Password (at least ${MIN_PASSWORD} characters)`, password),
    h("label", {}, "Password again", again),
    submit,
  );
  shell.app.replaceChildren(
    shell.header(null),
    h(
      "main.auth",
      {},
      h("h1", {}, "Register"),
      h("p.muted", {}, "With the token you were given: it invites you, or lets you set a new password."),
      error.el,
      form,
      h("p.muted.small", {}, "Already have an account? ", h("a", { href: "/#/signin" }, "Sign in"), "."),
    ),
  );
  if (initialToken) {
    void lookUp();
    email.focus();
  } else token.focus();
}

export function renderAccount(shell: Shell) {
  const me = shell.me();
  if (!me) return;
  const error = errorLine();
  const done = h("p.ok", { hidden: true, role: "status" });
  const say = (text: string) => {
    done.textContent = text;
    done.hidden = false;
    error.hide();
  };
  const parts: Node[] = [
    h(
      "dl.facts",
      {},
      h("dt", {}, "Name"),
      h("dd", {}, me.name),
      h("dt", {}, "Role"),
      h("dd", {}, me.role === "admin" ? "Administrator" : "Guest"),
      h("dt", {}, "Signs in with"),
      h("dd", {}, me.oidc ? "the sign-in provider" : (me.email ?? "an e-mail address")),
    ),
  ];
  if (me.oidc) {
    parts.push(h("p.muted", {}, "Your name and password are your sign-in provider's: change them there."));
  } else {
    const name = h("input", { value: me.name, maxLength: 64, required: true, "aria-label": "Name" });
    const current = h("input", { type: "password", autocomplete: "current-password", "aria-label": "Current password" });
    const next = h("input", {
      type: "password",
      autocomplete: "new-password",
      minLength: MIN_PASSWORD,
      "aria-label": "New password",
    });
    const again = h("input", { type: "password", autocomplete: "new-password", "aria-label": "New password again" });
    parts.push(
      h(
        "form.auth-form",
        {
          onsubmit: async (e: Event) => {
            e.preventDefault();
            try {
              const user = await api.updateMe({ name: name.value });
              shell.signedIn(user, "#/account");
              say("Name changed.");
            } catch (err) {
              error.show(message(err));
            }
          },
        },
        h("h2", {}, "Name"),
        h("label", {}, "Name (others see it)", name),
        h("button.primary", { type: "submit" }, "Change the name"),
      ),
      h(
        "form.auth-form",
        {
          onsubmit: async (e: Event) => {
            e.preventDefault();
            if (next.value !== again.value) return error.show("The two new passwords differ.");
            try {
              await api.updateMe({ password: next.value, currentPassword: current.value });
              current.value = next.value = again.value = "";
              say("Password changed: your other sessions are signed out.");
            } catch (err) {
              error.show(message(err));
            }
          },
        },
        h("h2", {}, "Password"),
        h("label", {}, "Current password", current),
        h("label", {}, `New password (at least ${MIN_PASSWORD} characters)`, next),
        h("label", {}, "New password again", again),
        h("button.primary", { type: "submit" }, "Change the password"),
      ),
    );
  }
  shell.app.replaceChildren(shell.header("account"), h("main.auth", {}, h("h1", {}, "Your account"), error.el, done, ...parts));
}
