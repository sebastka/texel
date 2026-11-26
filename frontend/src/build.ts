// The build the page comes from, in its footer: the commit, linked to the
// repository (set by scripts/build-env.sh).

import { h } from "./dom";

export function showBuild(footer: HTMLElement) {
  const commit = String(import.meta.env.VITE_COMMIT ?? "");
  const repository = String(import.meta.env.VITE_REPOSITORY ?? "");
  if (!commit) {
    footer.replaceChildren("Build: unknown");
    return;
  }
  footer.replaceChildren(
    "Build ",
    h("a.external.mono", { href: `${repository}/commit/${commit}`, target: "_blank", rel: "noreferrer" }, commit.slice(0, 7)),
    { changes: " with local changes", unknown: ", local changes unknown" }[String(import.meta.env.VITE_DIRTY ?? "")] ?? "",
  );
}
