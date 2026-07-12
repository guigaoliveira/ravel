#!/usr/bin/env bash
# Layer-3 live smoke: does a real coding agent connect to Ravel's MCP server and call a tool?
#
# NOT a CI test — needs the agent CLI installed + auth/network. Opt-in, per agent.
#
# Usage:  scripts/agent_smoke.sh <claude|codex|opencode|gemini|cursor-agent>
#
# It builds a tiny TS fixture with a known call graph (UserService --calls--> log),
# indexes it with Ravel, wires Ravel's MCP into the agent (project-local), then asks the
# agent — via headless/non-interactive mode — "who calls `log`?". PASS if the answer names
# `UserService` (proving the agent reached Ravel's symbol graph, not just guessed).
set -uo pipefail

AGENT="${1:-claude}"
RAVEL="$(cd "$(dirname "$0")/.." && pwd)/target/release/ravel"
[[ -x "$RAVEL" ]] || { echo "build release first: cargo build --release" >&2; exit 2; }
command -v "$AGENT" >/dev/null || { echo "agent '$AGENT' not installed" >&2; exit 3; }

FIX="$(mktemp -d)/proj"; mkdir -p "$FIX/src"
trap 'rm -rf "$(dirname "$FIX")"' EXIT
cat > "$FIX/src/service.ts" <<'EOF'
export function log() {}
export class UserService {
  save() { log(); }
}
EOF
cat > "$FIX/src/main.ts" <<'EOF'
import { UserService } from './service';
export function main() { new UserService().save(); }
EOF

( cd "$FIX" && "$RAVEL" index >/dev/null 2>&1 ) || { echo "index failed" >&2; exit 4; }
# Project-local MCP wiring so we don't touch the user's real global config.
( cd "$FIX" && "$RAVEL" install --target "$AGENT" --location local --yes >/dev/null 2>&1 ) \
  || echo "warn: install --location local may not support $AGENT; agent may need global" >&2

PROMPT='Use the ravel MCP tools (not grep) to find which symbols call the function `log` in this repo. Answer with just the caller name(s).'
echo "── running $AGENT headless in $FIX ──"

OUT=""
case "$AGENT" in
  claude)     OUT="$(cd "$FIX" && timeout 120 claude -p "$PROMPT" --dangerously-skip-permissions --output-format text 2>&1)";;
  codex)      OUT="$(cd "$FIX" && timeout 120 codex exec "$PROMPT" 2>&1)";;
  opencode)   OUT="$(cd "$FIX" && timeout 120 opencode run "$PROMPT" 2>&1)";;
  gemini)     OUT="$(cd "$FIX" && timeout 120 gemini -p "$PROMPT" 2>&1)";;
  cursor-agent) OUT="$(cd "$FIX" && timeout 120 cursor-agent -p "$PROMPT" 2>&1)";;
  *) echo "no headless recipe for $AGENT" >&2; exit 5;;
esac

echo "── agent output ──"; echo "$OUT" | tail -20
echo "──────────────────"
if echo "$OUT" | grep -q "UserService"; then
  echo "PASS: $AGENT reached the Ravel symbol graph (answer names UserService)"
  exit 0
else
  echo "FAIL/INCONCLUSIVE: 'UserService' not in answer (auth? trust prompt? agent used grep? see output above)"
  exit 1
fi
