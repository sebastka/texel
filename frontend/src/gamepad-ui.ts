// The gamepad chip of the player's toolbar, and the dialog that sets up a
// gamepad's layout one control at a time.

import { Learner, mappingFor, padName, saveMapping, savedMapping } from "./gamepad";

function node<K extends keyof HTMLElementTagNameMap>(tag: K, cls: string, text = ""): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text) e.textContent = text;
  return e;
}

function firstPad(): Gamepad | null {
  // Missing on insecure origins (plain http, but for localhost).
  return (navigator.getGamepads?.() ?? []).find((p): p is Gamepad => p !== null && p.connected) ?? null;
}

/**
 * A toolbar button showing the first gamepad: its name, or a warning when
 * the browser does not know its layout and none was set up. Clicking it
 * opens the setup dialog. `suspend` stops game input while it is open.
 */
export function gamepadChip(suspend: (on: boolean) => void): HTMLButtonElement {
  const chip = node("button", "ghost pad-chip");
  chip.hidden = true;
  const refresh = () => {
    if (!chip.isConnected) {
      clearInterval(timer);
      return;
    }
    const pad = firstPad();
    chip.hidden = !pad;
    if (!pad) return;
    const { known, custom } = mappingFor(pad);
    chip.classList.toggle("warn", !known);
    chip.textContent = known ? `Gamepad: ${padName(pad)}${custom ? " (custom)" : ""}` : "Set up gamepad";
    chip.title = !known
      ? `${padName(pad)}: the browser does not know its layout, so buttons may be wrong. Click to set it up.`
      : custom
        ? "Uses the layout you set up. Click to change it."
        : "Uses the browser's standard layout. Click to set up another one.";
  };
  // Browsers only reveal gamepads once a button is pressed: keep checking.
  const timer = setInterval(refresh, 1000);
  window.addEventListener("gamepadconnected", refresh);
  window.addEventListener("gamepaddisconnected", refresh);
  chip.addEventListener("click", () => {
    const pad = firstPad();
    if (pad) openSetup(pad, suspend, refresh);
  });
  return chip;
}

function openSetup(pad: Gamepad, suspend: (on: boolean) => void, onClose: () => void) {
  suspend(true);
  const learner = new Learner();
  const backdrop = node("div", "dialog-backdrop");
  const dialog = node("div", "dialog");
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-modal", "true");
  const title = node("h2", "", `Set up ${padName(pad)}`);
  const intro = node(
    "p",
    "muted small",
    "Press each control when asked, or skip it. Keep the other controls still: the first thing that moves is recorded.",
  );
  const prompt = node("p", "setup-prompt");
  const progress = node("p", "muted small");
  const actions = node("div", "dialog-actions");
  const skip = node("button", "ghost", "Skip");
  const save = node("button", "primary", "Save");
  const reset = node("button", "ghost", "Use the browser's layout");
  const cancel = node("button", "ghost", "Cancel");
  reset.hidden = !savedMapping(pad.id);
  actions.append(reset, cancel, skip, save);
  dialog.append(title, intro, prompt, progress, actions);
  backdrop.append(dialog);
  document.body.append(backdrop);

  let frame = 0;
  const close = () => {
    cancelAnimationFrame(frame);
    window.removeEventListener("keydown", onKey, true);
    backdrop.remove();
    suspend(false);
    onClose();
  };
  const render = () => {
    const step = learner.steps[learner.index];
    const set = Object.keys(learner.mapping.buttons).length + Object.keys(learner.mapping.sticks).length;
    if (step) {
      prompt.textContent = step.kind === "stick" ? step.label : `Press ${step.label}`;
      progress.textContent = `Step ${learner.index + 1} of ${learner.steps.length} · ${set} set so far`;
    } else {
      prompt.textContent = "Done";
      progress.textContent = `${set} of ${learner.steps.length} controls set. Save to use this layout for this model of gamepad, in this browser.`;
    }
    skip.hidden = learner.done;
    save.hidden = !learner.done;
  };
  const tick = () => {
    const current = navigator.getGamepads?.()[pad.index];
    if (!current?.connected) {
      progress.textContent = "The gamepad was disconnected.";
      return;
    }
    if (learner.update(current)) render();
    frame = requestAnimationFrame(tick);
  };
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      close();
    }
  };
  skip.addEventListener("click", () => {
    learner.skip();
    render();
  });
  save.addEventListener("click", () => {
    saveMapping(pad.id, learner.mapping);
    close();
  });
  reset.addEventListener("click", () => {
    saveMapping(pad.id, null);
    close();
  });
  cancel.addEventListener("click", close);
  window.addEventListener("keydown", onKey, true);
  render();
  frame = requestAnimationFrame(tick);
}
