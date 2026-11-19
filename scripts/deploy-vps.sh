#!/usr/bin/env bash
# AirRadar VPS deployment helper.
#
# Runs on the Docker Swarm manager over SSH (see .github/workflows/deploy.yml).
# Required env: SCHEDULE_IMAGE, TRAEFIK_HOST.
# Optional env: STACK_NAME, DEPLOY_PATH, TRAEFIK_ENTRYPOINTS, TRAEFIK_CERTRESOLVER,
#               TRAEFIK_NETWORK, GHCR_USER, GHCR_TOKEN, UPDATE_SECRETS,
#               ADMIN_TOKEN, BRAVE_API_KEY, TAVILY_API_KEY, DEEPSEEK_API_KEY,
#               LLM_API_KEY, LLM_SESSION_ID, DEEPSEEK_SEARCH_URL, DEEPSEEK_SEARCH_MODEL,
#               and the tuning variables used by docker-stack.yml.
set -euo pipefail

log() { printf '%s %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*"; }
die() { log "ERROR: $*"; exit 1; }

: "${STACK_NAME:=airradar}"
: "${SECRET_PREFIX:=airradar}"
: "${DEPLOY_PATH:=$(pwd)}"
: "${SCHEDULE_IMAGE:?SCHEDULE_IMAGE is required (e.g. ghcr.io/moderras/airradar:sha-ab12cd3)}"
: "${TRAEFIK_HOST:?TRAEFIK_HOST is required (e.g. schedule.example.com)}"
# Normalise TRAEFIK_HOST: tolerate values pasted with scheme, path, port,
# quotes or whitespace so the Traefik Host() rule never gets an invalid hostname.
TRAEFIK_HOST="$(printf '%s' "$TRAEFIK_HOST" | tr -d '[:space:]')"
DQ='"'
SQ="'"
TRAEFIK_HOST="${TRAEFIK_HOST//$DQ/}"
TRAEFIK_HOST="${TRAEFIK_HOST//$SQ/}"
TRAEFIK_HOST="${TRAEFIK_HOST#http://}"
TRAEFIK_HOST="${TRAEFIK_HOST#https://}"
TRAEFIK_HOST="${TRAEFIK_HOST%%/*}"
TRAEFIK_HOST="${TRAEFIK_HOST%:*}"
TRAEFIK_HOST="$(printf '%s' "$TRAEFIK_HOST" | tr '[:upper:]' '[:lower:]')"
TRAEFIK_HOST="${TRAEFIK_HOST%.}"
case "$TRAEFIK_HOST" in
  ""|*[!a-z0-9.-]*|.*|*.|*..*) die "TRAEFIK_HOST must be a bare hostname like schedule.example.com (no scheme, path, port, quotes or spaces)" ;;
esac
[[ "$TRAEFIK_HOST" == *.* ]] || die "TRAEFIK_HOST must be a fully qualified domain name (e.g. schedule.example.com)"
export TRAEFIK_HOST
: "${TRAEFIK_ENTRYPOINTS:=websecure}"
: "${TRAEFIK_CERTRESOLVER:=letsencrypt}"
: "${TRAEFIK_NETWORK:=traefik-public}"
: "${UPDATE_SECRETS:=0}"

# Variables consumed by docker stack deploy for compose interpolation.
export STACK_NAME SECRET_PREFIX SCHEDULE_IMAGE TRAEFIK_HOST
export TRAEFIK_ENTRYPOINTS TRAEFIK_CERTRESOLVER TRAEFIK_NETWORK
export BANGUMI_UA="${BANGUMI_UA:-AirRadar/0.1.0 (+https://github.com/ModerRAS/AirRadar)}"
export REFRESH_HOURS="${REFRESH_HOURS:-6}"
export MAX_SUBJECTS_PER_RUN="${MAX_SUBJECTS_PER_RUN:-30}"
export MAX_SEARCH_CALLS="${MAX_SEARCH_CALLS:-60}"
export MAX_LLM_CALLS="${MAX_LLM_CALLS:-10}"
export SEARCH_PROVIDER_ORDER="${SEARCH_PROVIDER_ORDER:-brave,tavily,deepseek}"
export INCLUDE_PLATFORMS="${INCLUDE_PLATFORMS:-TV,WEB}"
export SEASON_OVERRIDE="${SEASON_OVERRIDE:-}"
export LOG_LEVEL="${LOG_LEVEL:-info}"
export LLM_BASE_URL="${LLM_BASE_URL:-}"
export LLM_MODEL="${LLM_MODEL:-}"
export LLM_SESSION_ID="${LLM_SESSION_ID:-}"
export DEEPSEEK_SEARCH_URL="${DEEPSEEK_SEARCH_URL:-https://api.deepseek.com/v1/chat/completions}"
export DEEPSEEK_SEARCH_MODEL="${DEEPSEEK_SEARCH_MODEL:-deepseek-chat}"
export CACHE_MAX_BYTES="${CACHE_MAX_BYTES:-268435456}"
export HISTORY_KEEP="${HISTORY_KEEP:-10}"

command -v docker >/dev/null 2>&1 || die "docker CLI not found"
docker info >/dev/null 2>&1 || die "cannot talk to the Docker daemon (is this the Swarm manager?)"

if [ -n "${GHCR_TOKEN:-}" ]; then
  log "logging in to ghcr.io as ${GHCR_USER:-github}"
  printf '%s' "${GHCR_TOKEN}" | docker login ghcr.io --username "${GHCR_USER:-github}" --password-stdin >/dev/null
fi

secret_name() { printf '%s_%s' "$SECRET_PREFIX" "$1"; }

secret_entries=(
  "admin_token:${ADMIN_TOKEN:-}"
  "brave_api_key:${BRAVE_API_KEY:-}"
  "tavily_api_key:${TAVILY_API_KEY:-}"
  "deepseek_api_key:${DEEPSEEK_API_KEY:-}"
  "llm_api_key:${LLM_API_KEY:-}"
)

ensure_secret() {
  local name value payload
  name="$(secret_name "$1")"
  value="$2"
  if docker secret inspect "$name" >/dev/null 2>&1; then
    log "secret $name exists (value unchanged)"
    return 0
  fi
  # Optional provider keys may be empty. Docker secrets must have a payload, so
  # store one space; AirRadar trims secret files and treats whitespace as unset.
  payload="$value"
  [ -n "$payload" ] || payload=" "
  printf '%s' "$payload" | docker secret create "$name" - >/dev/null
  log "secret $name created"
}

wait_for_stack_removal() {
  local attempt
  for attempt in $(seq 1 60); do
    if [ -z "$(docker stack services "$STACK_NAME" --quiet 2>/dev/null || true)" ]; then
      return 0
    fi
    sleep 2
  done
  die "stack $STACK_NAME did not finish removing within 120s"
}

if [ "$UPDATE_SECRETS" = "1" ]; then
  log "UPDATE_SECRETS=1: removing stack so secrets can be rotated (brief downtime)"
  docker stack rm "$STACK_NAME" >/dev/null 2>&1 || true
  wait_for_stack_removal
  for entry in "${secret_entries[@]}"; do
    name="${entry%%:*}"
    docker secret rm "$(secret_name "$name")" >/dev/null 2>&1 || true
  done
fi

for entry in "${secret_entries[@]}"; do
  ensure_secret "${entry%%:*}" "${entry#*:}"
done

cd "$DEPLOY_PATH"
[ -f docker-stack.yml ] || die "docker-stack.yml not found in $DEPLOY_PATH (upload it first)"
log "deploying $SCHEDULE_IMAGE to stack $STACK_NAME"
docker stack deploy --with-registry-auth --prune -c docker-stack.yml "$STACK_NAME"

log "waiting for service to settle"
for attempt in $(seq 1 30); do
  if docker service inspect "$(printf '%s_airradar' "$STACK_NAME")" >/dev/null 2>&1; then
    break
  fi
  sleep 2
done
docker stack services "$STACK_NAME" || true
docker service ps "$(printf '%s_airradar' "$STACK_NAME")" --no-trunc \
  --format '{{.Name}} {{.CurrentState}} {{.Error}}' 2>/dev/null | head -n 5 || true
log "deployment finished; check https://$TRAEFIK_HOST/health"
