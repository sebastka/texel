#!/bin/sh
# Runs a command with the build, for the pages' footer, in Vite's
# environment: VITE_COMMIT (TEXEL_COMMIT, as the image's build passes it,
# or git's HEAD), VITE_DIRTY ("changes" when the tree had some, "unknown"
# when git could not tell, as in a copy missing objects; else empty) and
# VITE_REPOSITORY (TEXEL_REPOSITORY, or the project's).
set -eu
if [ -n "${TEXEL_COMMIT:-}" ]; then
  VITE_COMMIT=$TEXEL_COMMIT
  VITE_DIRTY=
else
  VITE_COMMIT=$(git rev-parse HEAD 2>/dev/null || true)
  if status=$(git status --porcelain 2>/dev/null); then
    VITE_DIRTY=${status:+changes}
  else
    VITE_DIRTY=unknown
  fi
fi
VITE_REPOSITORY=${TEXEL_REPOSITORY:-https://github.com/sebastka/texel}
export VITE_COMMIT VITE_DIRTY VITE_REPOSITORY
exec "$@"
