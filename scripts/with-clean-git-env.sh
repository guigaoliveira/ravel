#!/bin/sh
# Hooks export GIT_DIR/GIT_INDEX_FILE and friends. Tests create other repositories;
# allowing those variables through redirects their init/add/commit into the caller.
set -eu
git_local_vars=$(git rev-parse --local-env-vars)
for git_local_var in $git_local_vars; do
    unset "$git_local_var"
done
exec "$@"
