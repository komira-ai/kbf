#!/usr/bin/env bash
# setup.sh: the one-time (and rerunnable) setup for the kbf gamma trigger of a repository
# (.github/workflows/gamma.yml and `kbf-installer deploy`). Every site-specific value is an argument.
#
# DRY RUN BY DEFAULT: prints every step and runs nothing. `--apply` runs the `gh` steps.
# Never forces, never deletes, holds no secret value, and does not call the Tailscale API or
# change a Tailscale ACL: those, and the host-side steps, are PRINTED for a person to do.
#
# Usage:
#   deploy/gamma/setup.sh --repo <owner/name> --host <frontend-host> --user <deploy-user>
#                  [--tag tag:kbf-gamma-ci] [--api <127.0.0.1:port>] [--token-file <path>] [--apply]
#
# What --apply does (gh only, against --repo):
#   1. creates the `gamma` environment (PUT is idempotent);
#   2. sets the repository variables GAMMA_HOST, GAMMA_USER, TS_TAG (the workflow's job
#      `if:` reads them, so it stays skipped until all three are set);
#   3. prompts (no echo) for the Tailscale OAuth client id and secret and stores them as
#      secrets of the `gamma` environment, straight from the prompt to `gh` on stdin.
#
# Always printed, never done: the Tailscale OAuth client, the ACL grant and SSH rule (a JSON
# block to paste into the policy file), the sudoers drop-in and the host's deploy config
# (docs: docs/deploy/gamma.md).
set -euo pipefail
set +x

REPO=""
HOST="" USER_NAME="" TAG="tag:kbf-gamma-ci" API="" TOKEN_FILE="" APPLY=0

die() { echo "setup.sh: $*" >&2; exit 2; }

while [ $# -gt 0 ]; do
  case "$1" in
    --repo) [ $# -ge 2 ] || die "--repo needs a value"; REPO="$2"; shift 2;;
    --host) [ $# -ge 2 ] || die "--host needs a value"; HOST="$2"; shift 2;;
    --user) [ $# -ge 2 ] || die "--user needs a value"; USER_NAME="$2"; shift 2;;
    --tag) [ $# -ge 2 ] || die "--tag needs a value"; TAG="$2"; shift 2;;
    --api) [ $# -ge 2 ] || die "--api needs a value"; API="$2"; shift 2;;
    --token-file) [ $# -ge 2 ] || die "--token-file needs a value"; TOKEN_FILE="$2"; shift 2;;
    --apply) APPLY=1; shift;;
    -h|--help) sed -n '2,22p' "$0"; exit 0;;
    *) die "unknown argument: $1";;
  esac
done

[ -n "$REPO" ] || die "--repo is required (owner/name of the repository holding gamma.yml)"
[[ "$REPO" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || die "--repo must look like owner/name"
[ -n "$HOST" ] || die "--host is required (the frontend host's Tailscale name)"
[ -n "$USER_NAME" ] || die "--user is required (the deploy user on that host)"
# A name, not an address: the workflow reaches the host by its Tailscale name.
if [[ "$HOST" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || [[ "$HOST" == *:* ]]; then
  die "--host must be a Tailscale name, not an IP address"
fi
[[ "$HOST" =~ ^[A-Za-z0-9._-]+$ ]] || die "--host has characters a host name does not"
[[ "$USER_NAME" =~ ^[a-z_][a-z0-9_-]*$ ]] || die "--user is not a valid account name"
[[ "$TAG" =~ ^tag:[a-z0-9-]+$ ]] || die "--tag must look like tag:name"
if [ -n "$API" ]; then
  [[ "$API" =~ ^127\.0\.0\.1:[0-9]+$ ]] || die "--api must be 127.0.0.1:<port> (the operator API takes writes from loopback only)"
fi
if [ -n "$TOKEN_FILE" ]; then
  [[ "$TOKEN_FILE" == /* ]] || die "--token-file must be an absolute path"
fi

say() { printf '%s\n' "$*"; }
run() { # print the command; run it only with --apply
  say "  \$ $*"
  if [ "$APPLY" = 1 ]; then "$@"; fi
}

say "== mode: $([ "$APPLY" = 1 ] && echo APPLY || echo 'DRY RUN (nothing runs; add --apply)')"
say "== repository: $REPO"

command -v gh >/dev/null || die "gh is not installed"
if ! gh auth status >/dev/null 2>&1; then
  if [ "$APPLY" = 1 ]; then die "gh is not logged in (gh auth login)"; fi
  say "!! gh is not logged in; --apply would refuse"
fi
if gh auth status >/dev/null 2>&1; then
  found=$(gh repo view "$REPO" --json nameWithOwner --jq .nameWithOwner 2>/dev/null || true)
  [ "$found" = "$REPO" ] || die "gh cannot see $REPO (got: '${found:-nothing}')"
fi

say
say "== 1. GitHub environment"
run gh api -X PUT "repos/$REPO/environments/gamma"

say
say "== 2. repository variables (the workflow stays skipped until all three are set)"
run gh variable set GAMMA_HOST --repo "$REPO" --body "$HOST"
run gh variable set GAMMA_USER --repo "$REPO" --body "$USER_NAME"
run gh variable set TS_TAG --repo "$REPO" --body "$TAG"

say
say "== 3. environment secrets (read without echo, piped to gh, never written to a file)"
if [ "$APPLY" = 1 ]; then
  [ -t 0 ] || die "the secrets are prompted for; run this in a terminal"
  # set +x again: nothing below may be traced.
  set +x
  read -rsp "Tailscale OAuth client id: " ts_id; echo
  [ -n "$ts_id" ] || die "empty client id"
  printf '%s' "$ts_id" | gh secret set TS_OAUTH_CLIENT_ID --env gamma --repo "$REPO"
  unset ts_id
  read -rsp "Tailscale OAuth client secret: " ts_secret; echo
  [ -n "$ts_secret" ] || die "empty client secret"
  printf '%s' "$ts_secret" | gh secret set TS_OAUTH_SECRET --env gamma --repo "$REPO"
  unset ts_secret
else
  say "  (would prompt twice, then: gh secret set TS_OAUTH_CLIENT_ID|TS_OAUTH_SECRET --env gamma --repo $REPO)"
fi

say
say "== 4. NOT DONE BY THIS SCRIPT: Tailscale admin console (a person does these)"
cat <<TS
  a. Settings > OAuth clients > Generate: scope "Auth Keys: Write" ONLY, restricted to the tag
     $TAG. Copy the client id and secret into the prompts above (step 3).
  b. Policy file: add the tag owner, a grant and an SSH rule (merge into your policy; do not
     replace it). The workflow's runner joins as $TAG and may SSH to the frontend host as
     $USER_NAME and nothing else:

     "tagOwners": { "$TAG": ["autogroup:admin"] },
     "grants": [
       { "src": ["$TAG"], "dst": ["$HOST"], "ip": ["22"] }
     ],
     "ssh": [
       { "action": "accept", "src": ["$TAG"], "dst": ["$HOST"], "users": ["$USER_NAME"] }
     ]

     ("dst" must name the frontend host as your policy names devices: its tag or its name.)
TS

say
HEX40=$(printf '[0-9a-f]%.0s' $(seq 1 40))
say "== 5. NOT DONE BY THIS SCRIPT: the frontend host (as root there)"
cat <<HOSTSTEPS
  a. The deploy account must exist: $USER_NAME (no password; it only receives the SSH command).
  b. Sudoers drop-in, installed with a syntax check first:
       printf '%s\n' '$USER_NAME ALL=(root) NOPASSWD: /usr/bin/kbf-installer deploy --apply --json --commit $HEX40 --bundle -' > /etc/sudoers.d/kbf-gamma.new
       visudo -c -f /etc/sudoers.d/kbf-gamma.new && install -m 0440 -o root -g root /etc/sudoers.d/kbf-gamma.new /etc/sudoers.d/kbf-gamma
     The rule pins the whole argument list the workflow sends; only the commit varies, and
     sudoers matches it against forty [0-9a-f] classes, so no other flag (--api, --token-file,
     --config, --lock) can be added. There is no wildcard. As well, kbf-installer deploy refuses
     those flags itself when sudo ran it (SUDO_UID is set), and takes the API address and token
     file only from the root-owned config in step c.
  c. The deploy config, root-owned, not writable by others (the deploy reads the API address
     and the token file from it, so the sudoers rule need not name them):
       install -d -m 0755 /etc/kbf-installer
       printf '{"api": "${API:-127.0.0.1:<port>}", "token_file": "${TOKEN_FILE:-<absolute path of the operator token file>}"}\n' > /etc/kbf-installer/deploy.json
       chmod 0644 /etc/kbf-installer/deploy.json
     The token file is the frontend's --api-token-file: owned by the server's user, mode 0600
     or 0400; the deploy runs as root and can read it.
  d. /run/kbf-installer must be creatable by root (the deploy's lock lives there).
HOSTSTEPS

say
say "== 6. NOT DONE BY THIS SCRIPT: repository settings"
say "  The repository's Actions policy must allow tailscale/github-action (the workflow lint already lists it)."
say "  Optional: add a required reviewer to the gamma environment to gate each deploy."
say
if [ "$APPLY" = 1 ]; then say "== done. Re-running is safe: the environment PUT and the variable and secret sets overwrite in place."; else say "== dry run complete. Re-run with --apply to do steps 1 to 3."; fi
