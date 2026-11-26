#!/bin/sh
# Runs a command with the build, for the pages' footer, in Vite's
# environment: VITE_COMMIT (TEXEL_COMMIT, as the image's build passes it,
# or git's HEAD), VITE_DIRTY (not empty when the tree had changes) and
# VITE_REPOSITORY (TEXEL_REPOSITORY, or the project's).
set -eu
if [ -n "${TEXEL_COMMIT:-}" ]; then
  VITE_COMMIT=$TEXEL_COMMIT
  VITE_DIRTY=
else
  VITE_COMMIT=$(git rev-parse HEAD 2>/dev/null || true)
  VITE_DIRTY=$(git status --porcelain 2>/dev/null | head -n 1 || true)
fi
VITE_REPOSITORY=${TEXEL_REPOSITORY:-https://github.com/sebastka/texel}
export VITE_COMMIT VITE_DIRTY VITE_REPOSITORY
exec "$@"
