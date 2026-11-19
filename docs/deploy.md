# AirRadar deployment guide (Docker Swarm + Traefik + GitHub Actions)

This guide deploys AirRadar to the existing x86_64 Debian VPS that runs
**Docker Swarm** and **Traefik**. It never installs systemd units: the service
runs as a Swarm service, the collector is an in-process tokio timer and
`POST /admin/refresh` / `airradar --once` are the manual hooks.

## 1. Target environment

- Single-node Swarm (manager = worker) is the recommended and tested topology.
- Traefik is already deployed with the Docker/Swarm provider and an external
  network shared by public services (the stack assumes `traefik-public` by
  default, configurable through `TRAEFIK_NETWORK`).
- A DNS `A/AAAA` record for the schedule hostname points at the VPS.
- The public GHCR image path is `ghcr.io/moderras/airradar`.

AirRadar publishes **no host ports**. Traefik reaches the container over the
Swarm routing mesh using the `traefik.http.services.airradar.loadbalancer.server.port=8080`
label.

## 2. Prerequisites checklist

| Item | Check |
| --- | --- |
| Swarm active | `docker info --format '{{.Swarm.LocalNodeState}}'` prints `active` |
| Traefik network exists | `docker network ls --filter name=traefik-public` |
| Repo is public + Actions enabled | GitHub repository settings |
| GHCR package exists | first push of `.github/workflows/docker.yml` creates it |
| DNS record | `dig +short schedule.example.com` |
| SSH access | `ssh root@VPS 'docker version'` works non-interactively |
| GHCR package visibility | public (recommended) or set `GHCR_TOKEN` secret |

## 3. Traefik values to fill

The stack interpolates these variables at deploy time (see
`scripts/deploy-vps.sh` and `docker-stack.yml`):

| Variable | Example | Meaning |
| --- | --- | --- |
| `TRAEFIK_HOST` | `schedule.example.com` | Host rule for the router |
| `TRAEFIK_ENTRYPOINTS` | `websecure` | Traefik entrypoint (use the TLS entrypoint) |
| `TRAEFIK_CERTRESOLVER` | `letsencrypt` | Certificate resolver name in your Traefik static config |
| `TRAEFIK_NETWORK` | `traefik-public` | External network shared with Traefik |
| `STACK_NAME` | `airradar` | Swarm stack name (keep it stable; changing it creates a new volume) |
| `DEPLOY_PATH` | `/opt/airradar` | Directory on the VPS holding `docker-stack.yml` and `scripts/deploy-vps.sh` |

If your Traefik listens on the `web` entrypoint and redirects to `websecure`,
set `TRAEFIK_ENTRYPOINTS=websecure`; if you terminate TLS elsewhere, drop
`tls=true` from the labels (or keep the resolver value consistent with your
Traefik static configuration).

## 4. Repository and image

1. Repository: `https://github.com/ModerRAS/AirRadar` (MIT, public).
2. Push to `main` → `docker.yml` runs `cargo test`, builds the image and pushes
   `ghcr.io/moderras/airradar:latest` plus `sha-<7 chars>` and branch/pr tags.
3. Make the GHCR package public once (GitHub → profile → Packages → airradar →
   Package settings → Change visibility → Public). If it stays private, set the
   `GHCR_TOKEN` secret so the VPS can pull; `deploy-vps.sh` performs
   `docker login ghcr.io` with it before `docker stack deploy --with-registry-auth`.

## 5. GitHub environment `production`

Create an environment named `production` in the repository settings
(Settings → Environments → New environment). Add the following **environment
secrets**:

| Secret | Required | Notes |
| --- | --- | --- |
| `SSH_HOST` | yes | Swarm manager host/IP |
| `SSH_USER` | yes | user allowed to run docker (`root` or docker-group user) |
| `SSH_KEY` | yes | private key contents (OpenSSH), no passphrase or agent-loaded |
| `SSH_PORT` | no | SSH port. **Must be set if your SSH port is not 22** (default `22`) |
| `GHCR_USER` | no | default: repository owner |
| `GHCR_TOKEN` | private image only | PAT with `read:packages` |
| `TRAEFIK_HOST` | yes | e.g. `schedule.example.com` |
| `TRAEFIK_ENTRYPOINTS` | no | default `websecure` |
| `TRAEFIK_CERTRESOLVER` | no | default `letsencrypt` |
| `TRAEFIK_NETWORK` | no | default `traefik-public` |
| `ADMIN_TOKEN` | yes | long random string for `POST /admin/refresh` |
| `BRAVE_API_KEY` | recommended | at least one search provider is strongly recommended |
| `TAVILY_API_KEY` | optional | |
| `DEEPSEEK_API_KEY` | optional | DeepSeek **search** key (independent from the OpenCode Go LLM key) |
| `LLM_API_KEY` | optional | OpenCode Go / OpenAI-compatible extraction key (independent from search keys) |

Optional **environment variables** (same effect as secrets, but non-secret):
`STACK_NAME`, `DEPLOY_PATH`, `BANGUMI_UA`, `REFRESH_HOURS`,
`MAX_SUBJECTS_PER_RUN`, `MAX_SEARCH_CALLS`, `MAX_LLM_CALLS`,
`SEARCH_PROVIDER_ORDER`, `INCLUDE_PLATFORMS`, `SEASON_OVERRIDE`,
`DEEPSEEK_SEARCH_URL`, `DEEPSEEK_SEARCH_MODEL`,
`LLM_BASE_URL`, `LLM_MODEL`, `LLM_SESSION_ID`, `LOG_LEVEL`, `CACHE_MAX_BYTES`,
`HISTORY_KEEP`, `TRAEFIK_HOST`/`TRAEFIK_ENTRYPOINTS`/`TRAEFIK_CERTRESOLVER`/`TRAEFIK_NETWORK`.

Generate the admin token with something like `openssl rand -hex 32`.

### 5.1 Credentials: search vs LLM (independent)

Two separate credential groups; AirRadar never mixes them:

| Purpose | GitHub secrets / variables | Notes |
| --- | --- | --- |
| Search / discovery | `SEARCH_PROVIDER_ORDER=deepseek`, `DEEPSEEK_API_KEY` (secret), optional `DEEPSEEK_SEARCH_URL` / `DEEPSEEK_SEARCH_MODEL` (vars), `BRAVE_API_KEY`, `TAVILY_API_KEY` | Each provider uses its own key; none of these enables the LLM |
| LLM extraction fallback | `LLM_BASE_URL` (var), `LLM_MODEL` (var), `LLM_SESSION_ID` (var, optional), `LLM_API_KEY` (secret) | Only `LLM_*`; never reuses search keys |

OpenCode Go (LLM side; OpenAI-compatible base):

```bash
LLM_BASE_URL=https://opencode.ai/inference/go/v1
LLM_API_KEY=<OpenCode Go subscription key>   # separate from DeepSeek search key
LLM_MODEL=<OpenCode Go model id>
LLM_SESSION_ID=                              # optional; auto-generated + persisted
```

The gateway requires `x-opencode-session` (HTTP 400 without it). AirRadar adds
the header automatically when the endpoint host contains `opencode.ai`. When
`LLM_SESSION_ID` is empty the service generates one and stores it as
`/data/llm-session-id` in the data volume, so it remains stable across
restarts and redeploys. The session id is not a secret and is never logged
together with the key.

DeepSeek search (search side):

```bash
SEARCH_PROVIDER_ORDER=deepseek
DEEPSEEK_API_KEY=<DeepSeek search key>
DEEPSEEK_SEARCH_URL=https://api.deepseek.com/v1/chat/completions
DEEPSEEK_SEARCH_MODEL=deepseek-chat
```

## 6. First deployment

Option A — CI (recommended): push to `main` or run the `deploy` workflow manually
(Actions → deploy → Run workflow, leave `image_tag` empty). The workflow:

1. builds and pushes `sha-<7>` + `latest` to GHCR;
2. SSHes to the VPS, creates the deploy directory;
3. uploads `docker-stack.yml` and `scripts/deploy-vps.sh`;
4. creates missing external docker secrets idempotently and passes
   `LLM_SESSION_ID` / `DEEPSEEK_SEARCH_URL` / `DEEPSEEK_SEARCH_MODEL` through;
5. runs `docker stack deploy --with-registry-auth --prune -c docker-stack.yml airradar`.

Option B — manual from the VPS (no CI), see section 10.

## 7. Verify

```bash
# On the VPS
docker stack services airradar
docker service ps airradar_airradar --no-trunc
docker service logs --tail 100 airradar_airradar

# From anywhere
curl -fsS "https://$TRAEFIK_HOST/health"
curl -fsS "https://$TRAEFIK_HOST/api/status"
curl -fsS "https://$TRAEFIK_HOST/api/schedule/latest" | head -c 400

# Manual collection (uses the admin token from a docker secret on the server)
curl -fsS -X POST -H "Authorization: Bearer $ADMIN_TOKEN" "https://$TRAEFIK_HOST/admin/refresh"
```

After the first deploy the data volume is empty and the scheduler runs a
startup collection after ~10 seconds. `/api/schedule/latest` can return 404 for
a minute or two during the very first run; `/api/status` shows `running: true`
and then the first `lastRun` summary.

## 8. Optional: run a one-shot collection as a Swarm job

For debugging without keeping a shell session:

```bash
docker service create --name airradar-once --rm \
  --network traefik-public \
  --mount type=volume,source=airradar_airradar_data,target=/data \
  --env DATA_DIR=/data --env MAX_SUBJECTS_PER_RUN=3 \
  --env BANGUMI_UA="AirRadar/0.1.0 (+https://github.com/ModerRAS/AirRadar)" \
  --restart-condition none \
  ghcr.io/moderras/airradar:latest --once
```

Do not run it while the resident service is refreshing: both write to the same
volume and state is merged, but concurrent runs are not coordinated across
containers.

## 9. Docker secrets

Five **external** secrets are referenced by the stack:

| Secret name | Env file in container | Source GitHub secret |
| --- | --- | --- |
| `airradar_admin_token` | `ADMIN_TOKEN_FILE` | `ADMIN_TOKEN` |
| `airradar_brave_api_key` | `BRAVE_API_KEY_FILE` | `BRAVE_API_KEY` |
| `airradar_tavily_api_key` | `TAVILY_API_KEY_FILE` | `TAVILY_API_KEY` |
| `airradar_deepseek_api_key` | `DEEPSEEK_API_KEY_FILE` | `DEEPSEEK_API_KEY` |
| `airradar_llm_api_key` | `LLM_API_KEY_FILE` | `LLM_API_KEY` |

`scripts/deploy-vps.sh` creates any missing secret (empty optional keys are
created as a single space, which AirRadar treats as unset). `LLM_SESSION_ID` is
not a docker secret: it is a routing id, and the generated value is persisted in
the data volume (`/data/llm-session-id`). Only `LLM_API_KEY` is mounted as a
secret. Create the secrets manually when deploying without CI:

```bash
printf '%s' "$ADMIN_TOKEN" | docker secret create airradar_admin_token -
printf '%s' "$BRAVE_API_KEY" | docker secret create airradar_brave_api_key -
printf '%s' "$TAVILY_API_KEY" | docker secret create airradar_tavily_api_key -
printf '%s' "$DEEPSEEK_API_KEY" | docker secret create airradar_deepseek_api_key -
printf '%s' "$LLM_API_KEY" | docker secret create airradar_llm_api_key -
```

Secrets are immutable in Docker. Rotating one means removing the stack, removing
and re-creating the secret, then deploying again:

- CI: Actions → deploy → Run workflow → `update_secrets = true` (brief downtime).
- Manual: see section 12.

## 10. Manual deployment (no GitHub Actions)

```bash
# 1) Build and push from a machine with Docker (or build on the VPS)
docker build -t ghcr.io/moderras/airradar:manual-1 .
echo "$GHCR_TOKEN" | docker login ghcr.io -u "$GHCR_USER" --password-stdin
docker push ghcr.io/moderras/airradar:manual-1

# 2) On the VPS: put docker-stack.yml and scripts/deploy-vps.sh in /opt/airradar
mkdir -p /opt/airradar
cd /opt/airradar
# copy the two files here (scp/git) then:
export SCHEDULE_IMAGE=ghcr.io/moderras/airradar:manual-1
export TRAEFIK_HOST=schedule.example.com
export TRAEFIK_ENTRYPOINTS=websecure
export TRAEFIK_CERTRESOLVER=letsencrypt
export TRAEFIK_NETWORK=traefik-public
export ADMIN_TOKEN='<long random token>'
export BRAVE_API_KEY='<key>'
export DEEPSEEK_API_KEY='<DeepSeek search key>'
# Optional LLM extraction fallback (OpenCode Go; keys are independent from search)
# export LLM_BASE_URL='https://opencode.ai/inference/go/v1'
# export LLM_MODEL='<OpenCode Go model id>'
# export LLM_API_KEY='<OpenCode Go subscription key>'
# export LLM_SESSION_ID=''   # empty = generated and persisted in /data
bash scripts/deploy-vps.sh
```

The script logs in to GHCR when `GHCR_TOKEN` is set, creates missing secrets,
deploys the stack and prints service status. Re-running it is idempotent.

## 11. Rollback

Every build pushes an immutable `sha-<7 chars>` tag; `latest` is a moving tag.

1. Find the last good tag: GitHub Actions run of `docker.yml` or
   `docker image ls` / package versions in GitHub Packages.
2. Roll back through CI: Actions → deploy → Run workflow →
   `image_tag = sha-<previous>`. The build is skipped, the existing image is
   deployed.
3. Or on the VPS:
   ```bash
   SCHEDULE_IMAGE=ghcr.io/moderras/airradar:sha-<previous> \
     TRAEFIK_HOST=schedule.example.com bash scripts/deploy-vps.sh
   ```
4. Swarm also rolls back automatically when a rolling update fails
   (`failure_action: rollback`), but the image reference is `latest`, so the
   explicit tag above is the reliable path.

Data (/data volume) is independent from the image, so rolling back the image
keeps collected state and published schedule.

## 12. Rotating a secret manually

```bash
cd /opt/airradar
docker stack rm airradar
for i in $(seq 1 60); do [ -z "$(docker stack services airradar -q 2>/dev/null)" ] && break; sleep 2; done
docker secret rm airradar_admin_token
printf '%s' "$NEW_ADMIN_TOKEN" | docker secret create airradar_admin_token -
bash scripts/deploy-vps.sh     # same env as the first deploy
```

## 13. Volume, permissions, data migration

- Data lives in the named volume `airradar_airradar_data` (stack name +
  volume name), mounted at `/data`.
- The image creates `/data` owned by uid/gid `10001`; a fresh named volume inherits
  that ownership. If you migrate an existing volume created by another image,
  fix ownership on the VPS:

  ```bash
  docker run --rm -v airradar_airradar_data:/data alpine \
    chown -R 10001:10001 /data
  ```

- Backup / restore:

  ```bash
  docker run --rm -v airradar_airradar_data:/data -v "$PWD":/backup alpine \
    tar czf /backup/airradar-data.tar.gz -C /data .
  # restore
  docker run --rm -v airradar_airradar_data:/data -v "$PWD":/backup alpine \
    sh -c 'cd /data && tar xzf /backup/airradar-data.tar.gz'
  ```

- Keep `STACK_NAME` stable: changing it deploys a new stack with a new empty
  volume.
- Multi-node Swarm: the volume is node-local. Keep `replicas: 1` and (optionally)
  pin the service with a `node.hostname` placement constraint, or use a shared
  volume driver.

## 14. Operations

| Task | Command |
| --- | --- |
| Logs | `docker service logs -f --tail 200 airradar_airradar` |
| Health | `curl -fsS https://$TRAEFIK_HOST/health` |
| Last run | `curl -fsS https://$TRAEFIK_HOST/api/status` |
| Trigger refresh | `curl -X POST -H "Authorization: Bearer $ADMIN_TOKEN" https://$TRAEFIK_HOST/admin/refresh` |
| Review queue | `docker run --rm -v airradar_airradar_data:/data alpine cat /data/review.md` |
| Disk usage | `docker system df -v` (volume size) |

Resource guards are configured in `docker-stack.yml`: memory limit 128 MB,
reservation 32 MB, CPU limit 0.50, json-file logs 10 MB x 3. The app itself caps
the page cache (256 MB LRU), history (10 versions) and state retention
(4 seasons), so steady-state disk stays well below 1 GB plus the image (~90 MB).

## 15. Troubleshooting

| Symptom | Likely cause | Fix |
| --- | --- | --- |
| Traefik returns 404 | router labels/network/entrypoint mismatch | check `docker service inspect airradar_airradar --format '{{json .Spec.Labels}}'`; verify service is on `traefik-public` and `TRAEFIK_ENTRYPOINTS` matches your TLS entrypoint |
| Traefik returns 502 | container unhealthy or wrong port | `docker service logs airradar_airradar`; check healthcheck; port must stay 8080 |
| Service fails to start: "secret not found" | external secret missing | run `scripts/deploy-vps.sh` again or create the secret manually |
| Startup fails reading a secret file | secret created empty | optional keys are fine (empty -> unset); for `ADMIN_TOKEN` create a real value |
| `/admin/refresh` returns 503 | `ADMIN_TOKEN` not configured in the container | check the `airradar_admin_token` secret; redeploy after fixing |
| `/admin/refresh` returns 401 | token mismatch | compare with the secret; remember `Authorization: Bearer <token>` |
| Every run publishes 0 items | no search provider keys (only Bangumi + official sites), or official sites are JS-only | configure `BRAVE_API_KEY` / `TAVILY_API_KEY` / `DEEPSEEK_API_KEY`; check `lastRun.search` and `lastRun.fetch` |
| OOM kill / restart loop | memory limit too small for concurrency | keep 128 MB; lower `FETCH_CONCURRENCY`, `FETCH_PAGES_PER_SUBJECT`, `MAX_SUBJECTS_PER_RUN` |
| GHCR pull denied on VPS | private package without login | add `GHCR_TOKEN`/`GHCR_USER` or make the package public |
| Deploy step can't find docker-stack.yml | upload failed / DEPLOY_PATH wrong | check the Actions log; the workflow creates the directory and re-uploads both files |
| Data volume permission denied | volume created by another uid | run the chown command from section 13 |

## 16. Security notes

- The container runs as non-root uid/gid 10001 with a read-only image layer.
- No host ports are published; only Traefik reaches the service.
- Secrets are mounted as files under `/run/secrets` and read once at startup.
- Admin refresh is protected by a bearer token; set the token to a long random
  value and rotate it if leaked (section 12).
- AirRadar sends only its own User-Agent and reads public pages; it never
  executes JavaScript from fetched pages.
