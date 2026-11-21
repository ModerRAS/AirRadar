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
export STACK_NAME SECRET_PREFIX SCHEDULE_IMAGE TRAEFIK_HOST AIRRADAR_NODE
export TRAEFIK_ENTRYPOINTS TRAEFIK_CERTRESOLVER TRAEFIK_NETWORK
export BANGUMI_UA="${BANGUMI_UA:-AirRadar/0.1.0 (+https://github.com/ModerRAS/AirRadar)}"
export REFRESH_HOURS="${REFRESH_HOURS:-6}"
export MAX_SUBJECTS_PER_RUN="${MAX_SUBJECTS_PER_RUN:-120}"
export MAX_SEARCH_CALLS="${MAX_SEARCH_CALLS:-400}"
export MAX_LLM_CALLS="${MAX_LLM_CALLS:-40}"
export SEASON_PADDING_DAYS="${SEASON_PADDING_DAYS:-10}"
export SEARCH_PROVIDER_ORDER="${SEARCH_PROVIDER_ORDER:-brave,tavily,deepseek}"
export SEARCH_QUERIES_PER_SUBJECT="${SEARCH_QUERIES_PER_SUBJECT:-3}"
export INCLUDE_PLATFORMS="${INCLUDE_PLATFORMS:-TV,WEB}"
export SEASON_OVERRIDE="${SEASON_OVERRIDE:-}"
export LOG_LEVEL="${LOG_LEVEL:-info}"
export LLM_BASE_URL="${LLM_BASE_URL:-}"
export LLM_MODEL="${LLM_MODEL:-}"
export LLM_SESSION_ID="${LLM_SESSION_ID:-}"
export DEEPSEEK_SEARCH_URL="${DEEPSEEK_SEARCH_URL:-https://api.deepseek.com/v1/tools/search}"
export DEEPSEEK_SEARCH_MODEL="${DEEPSEEK_SEARCH_MODEL:-deepseek-chat}"
export CACHE_MAX_BYTES="${CACHE_MAX_BYTES:-268435456}"
export HISTORY_KEEP="${HISTORY_KEEP:-10}"
export PUBLISH_WEEKDAY_ONLY="${PUBLISH_WEEKDAY_ONLY:-true}"
export DEEPSEEK_SEARCH_COUNT="${DEEPSEEK_SEARCH_COUNT:-12}"
export DEEPSEEK_FETCH_URL="${DEEPSEEK_FETCH_URL:-https://api.deepseek.com/v1/tools/fetch}"
export MAX_DEEPSEEK_FETCH_CALLS="${MAX_DEEPSEEK_FETCH_CALLS:-150}"
export LLM_BATCH_SIZE="${LLM_BATCH_SIZE:-8}"

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

secret_fingerprint() { printf '%s' "$1" | sha256sum | cut -d' ' -f1; }
fingerprint_file="$DEPLOY_PATH/.secret-fingerprints"

rotate_secrets() {
  log "$1: removing stack so secrets can be rotated (brief downtime)"
  docker stack rm "$STACK_NAME" >/dev/null 2>&1 || true
  wait_for_stack_removal
  local entry
  for entry in "${secret_entries[@]}"; do
    docker secret rm "$(secret_name "${entry%%:*}")" >/dev/null 2>&1 || true
  done
}

rotate=0
if [ "$UPDATE_SECRETS" = "1" ]; then
  rotate=1
elif [ ! -f "$fingerprint_file" ]; then
  # First deploy with fingerprint tracking: sync once so pre-existing secrets
  # (created by older versions of this script) match the current values.
  log "no secret fingerprints yet; synchronising all secrets once"
  rotate=1
else
  for entry in "${secret_entries[@]}"; do
    name="${entry%%:*}"
    current="$(secret_fingerprint "${entry#*:}")"
    recorded="$(grep -E "^${name}=" "$fingerprint_file" 2>/dev/null | head -n1 | cut -d= -f2-)"
    if [ "$recorded" != "$current" ]; then
      log "secret $(secret_name "$name") changed since the last deploy"
      rotate=1
    fi
  done
fi

if [ "$rotate" = "1" ]; then
  rotate_secrets "secret rotation"
fi

for entry in "${secret_entries[@]}"; do
  ensure_secret "${entry%%:*}" "${entry#*:}"
done

# Persist value fingerprints (never the values themselves) so the next
# deploy can detect GitHub secret changes and rotate automatically.
mkdir -p "$DEPLOY_PATH"
: > "$fingerprint_file"
for entry in "${secret_entries[@]}"; do
  printf '%s=%s\n' "${entry%%:*}" "$(secret_fingerprint "${entry#*:}")" >> "$fingerprint_file"
done
chmod 600 "$fingerprint_file" 2>/dev/null || true

cd "$DEPLOY_PATH"
[ -f docker-stack.yml ] || die "docker-stack.yml not found in $DEPLOY_PATH (upload it first)"

secret_fingerprint_short() { printf '%s' "$1" | sha256sum | cut -c1-8; }
log "traefik config: host_len=${#TRAEFIK_HOST} host_sha=$(secret_fingerprint_short "$TRAEFIK_HOST") entrypoint=${TRAEFIK_ENTRYPOINTS} resolver=${TRAEFIK_CERTRESOLVER} network=${TRAEFIK_NETWORK}"
if [ -z "$TRAEFIK_HOST" ]; then die "TRAEFIK_HOST is empty after normalisation"; fi
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
rule="$(docker service inspect "$(printf '%s_airradar' "$STACK_NAME")" \
  --format '{{range $k, $v := .Spec.Labels}}{{$k}}={{$v}}{{"\n"}}{{end}}' 2>/dev/null \
  | grep 'traefik.http.routers.airradar.rule' || true)"
log "rendered $rule"
case "$rule" in
  *'Host(`)'*|*'Host(``)'*|*"Host()"*) die "Traefik Host() rule rendered without a domain; check TRAEFIK_HOST on the server" ;;
esac
# Wait for the service to converge; if it does not, print task state, service
# logs and disk usage so a failed rollout is diagnosable from CI output.
converged=0
for attempt in $(seq 1 60); do
  state="$(docker service ps "$(printf '%s_airradar' "$STACK_NAME")" --no-trunc --format '{{.Name}} {{.CurrentState}} {{.Error}}' 2>/dev/null | head -n 1 || true)"
  if printf '%s' "$state" | grep -q ' Running '; then
    converged=1
    break
  fi
  sleep 2
done
if [ "$converged" != "1" ]; then
  log "service did not converge; diagnostics follow"
  docker service ps "$(printf '%s_airradar' "$STACK_NAME")" --no-trunc 2>&1 || true
  service="$(printf '%s_airradar' "$STACK_NAME")"
  for task in $(docker service ps "$service" -q --no-trunc 2>/dev/null | head -n 4); do
    docker inspect --format 'task {{.ID}} node={{.NodeID}} state={{.Status.State}} err={{.Status.Err}} exit={{.Status.ContainerStatus.ExitCode}} container={{.Status.ContainerStatus.ContainerID}}' "$task" 2>&1 || true
    cid="$(docker inspect --format '{{.Status.ContainerStatus.ContainerID}}' "$task" 2>/dev/null || true)"
    if [ -n "$cid" ]; then docker logs --tail 80 "$cid" 2>&1 || true; fi
  done
  docker service logs --raw --tail 300 "$service" 2>&1 || true
  df -h / /var/lib/docker 2>/dev/null || true
  docker system df 2>/dev/null || true
  die "service failed to converge (see task state/logs above)"
fi
log "service converged: $state"
log "deployment finished; check https://$TRAEFIK_HOST/health"
