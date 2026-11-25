// Keeps the screen on while a game runs (the Screen Wake Lock API): with a
// gamepad, the system sees no input (only keyboards and mice count), and
// would dim the screen, lock it or sleep in the middle of a game. The
// browser drops the lock when the page is hidden; the player pauses then,
// and takes it again when it runs again. Where the API is missing (older
// browsers, or a page not served securely), nothing happens.

let lock: WakeLockSentinel | null = null;
/** The screen should stay on (a game runs). */
let wanted = false;

async function take() {
  if (!wanted || lock || document.hidden || !("wakeLock" in navigator)) return;
  try {
    const sentinel = await navigator.wakeLock.request("screen");
    if (!wanted) {
      void sentinel.release();
      return;
    }
    lock = sentinel;
    // Released by the browser (the page was hidden): taken again when
    // wanted and visible.
    sentinel.addEventListener("release", () => {
      if (lock === sentinel) lock = null;
    });
  } catch (e) {
    // Refused (battery saver, a policy): the system's own timers apply.
    console.warn("The screen may turn off while playing: the wake lock was refused.", e);
  }
}

document.addEventListener("visibilitychange", () => void take());

/** Keeps the screen on (`true`, while a game runs) or lets it go. */
export function keepScreenOn(on: boolean) {
  wanted = on;
  if (on) void take();
  else if (lock) {
    const sentinel = lock;
    lock = null;
    void sentinel.release();
  }
}
