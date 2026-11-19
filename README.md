# AirRadar — self-hosted anime airing schedule index

AirRadar answers one question for [MiruPlay](https://github.com/ModerRAS/MiruPlay):
**when does the current anime season air in Japan?** It collects the season list
from the official Bangumi API, searches the web through pluggable providers,
fetches and parses schedule pages, cross-checks independent sources and
publishes a small JSON schedule with a JST weekday + clock time.

- No third-party app backend, no headless browser, no database server.
- Rules-first extraction with an optional batched LLM fallback.
- Memory-capped Docker Swarm service (128 MB deploy limit) with a tiny HTTP API.
- MIT licensed; the repository contains **no secrets**.

## How it works

```
Bangumi API  鈹€鈹€鈻? subject list (season, id/name/name_cn/air_date/images)
      鈹?      鈻?search providers (Brave / Tavily / DeepSeek, ordered fallback)
      鈹? snippets + candidate URLs
      鈻?HTTP fetch (rate limited, 4 concurrent, 15 s timeout, 200 KB cap, disk cache)
      鈹? HTML -> plain text
      鈻?rule extraction (JP/ZH/EN patterns)  鈹€鈹€鈻? optional LLM fallback (batched)
      鈹?      鈻?cross-source verification (independence = domain)  鈹€鈹€鈻? confidence + review queue
      鈹?      鈻?/data/state.json, /data/schedule/<season>.json, /data/review.{json,md}
```

The service is a **resident web service** (Swarm service with
restart_policy: any + healthcheck). The collector is **not a separate daemon**:
the same process runs a tokio interval timer (`REFRESH_HOURS`, jitter 卤10%),
exposes `POST /admin/refresh`, and supports a one-shot `--once` CLI mode for
manual jobs and debugging.

## HTTP API

| Method | Path | Description |
| --- | --- | --- |
| GET | `/health` | Liveness + version + uptime (no auth) |
| GET | `/api/schedule/latest` | Most recently published schedule |
| GET | `/api/schedule?season=2026-fall` | Schedule for a specific season |
| GET | `/api/status` | Last run, next run, provider/cache stats, data version |
| POST | `/admin/refresh[?season=2026-fall]` | Trigger a background collection (202) |

`POST /admin/refresh` requires `Authorization: Bearer $ADMIN_TOKEN`; it returns
401 when the token is missing/invalid, 400 for an invalid season, 409 while
another run is active and 202 when accepted. See [docs/api.md](docs/api.md) for
the full payload schema and examples.

Minimal client use:

```bash
curl -fsS https://schedule.example.com/api/schedule/latest
```

## Quick start (local)

```bash
# Rust 1.85+ (developed with 1.99); no database or browser required.
cargo test
cargo run -- --once --max-subjects 3      # one collection, pretty JSON summary
cargo run                                 # web service on :8080
curl -fsS http://127.0.0.1:8080/health
```

Configuration is environment based; copy `.env.example` for a full reference.
Data is written to `DATA_DIR` (default `./data`, gitignored).

## Configuration

Every secret supports the docker-secret pattern: set `NAME_FILE=/path/to/file`
instead of `NAME` (whitespace-only files are treated as unset).

| Variable | Default | Description |
| --- | --- | --- |
| `PORT` | `8080` | HTTP listen port |
| `DATA_DIR` | `./data` | State, schedule, review, history, LLM cache |
| `CACHE_DIR` | `$DATA_DIR/cache` | Fetched page cache |
| `LOG_LEVEL` | `info` | `error`, `warn`, `info`, `debug` (stdout only) |
| `ADMIN_TOKEN` / `ADMIN_TOKEN_FILE` | unset | Bearer token for `/admin/refresh`; unset disables the endpoint |
| `BANGUMI_UA` | `AirRadar/0.1.0 (+repo URL)` | User-Agent for api.bgm.tv (required by Bangumi) |
| `FETCH_UA` | `BANGUMI_UA` | User-Agent for fetched pages |
| `SEASON_OVERRIDE` | current JST season | Force a season, e.g. `2026-fall` |
| `REFRESH_HOURS` | `6` | Collection interval (fractional allowed) |
| `MAX_SUBJECTS_PER_RUN` | `30` | Processing window per run (rotates by last-checked) |
| `INCLUDE_PLATFORMS` | `TV,WEB` | Bangumi platforms to include (`ALL` = no filter) |
| `SEARCH_PROVIDER_ORDER` | `brave,tavily,deepseek` | Search backends in fallback order; only providers with a key are enabled |
| `SEARCH_QUERIES_PER_SUBJECT` | `2` | Search queries issued per subject |
| `MAX_SEARCH_CALLS` | `60` | Search-query budget per run |
| `SEARCH_FALLBACK_ON_EMPTY` | `false` | Also try the next provider when a provider returns zero results |
| `SEARCH_TIMEOUT_SECS` | `20` | Per search request timeout |
| `BRAVE_API_KEY` / `_FILE` | unset | Brave Search API key |
| `TAVILY_API_KEY` / `_FILE` | unset | Tavily Search API key |
| `DEEPSEEK_API_KEY` / `_FILE` | unset | DeepSeek search key (OpenAI-compatible chat endpoint) |
| `DEEPSEEK_SEARCH_URL` | `https://api.deepseek.com/v1/chat/completions` | DeepSeek search endpoint |
| `DEEPSEEK_SEARCH_MODEL` | `deepseek-chat` | DeepSeek search model |
| `LLM_BASE_URL` | unset | OpenAI-compatible extraction fallback base URL |
| `LLM_API_KEY` / `_FILE` | unset | Extraction fallback API key |
| `LLM_MODEL` | unset | Extraction fallback model (all three LLM_* must be set) |
| `LLM_BATCH_SIZE` | `5` | Subjects per LLM request |
| `MAX_LLM_CALLS` | `10` | LLM request budget per run |
| `LLM_MAX_INPUT_CHARS` | `1200` | Text sent per subject to the LLM |
| `FETCH_TIMEOUT_SECS` | `15` | Page fetch timeout |
| `FETCH_CONCURRENCY` | `4` | Max concurrent page fetches (clamped 1-8) |
| `FETCH_MIN_INTERVAL_MS` | `300` | Global minimum interval between page fetches |
| `FETCH_MAX_BYTES` | `204800` | Downloaded bytes per page |
| `FETCH_PAGES_PER_SUBJECT` | `3` | Candidate pages fetched per subject |
| `ENABLE_OFFICIAL_SITE` | `true` | Also fetch the official website from the Bangumi infobox |
| `CACHE_MAX_BYTES` | `268435456` | Page cache budget (256 MB, LRU eviction) |
| `CACHE_TTL_HOURS` | `72` | Cache entry freshness (`0` = never expire) |
| `HISTORY_KEEP` | `10` | Published history versions kept on disk |
| `STATE_RETENTION_SEASONS` | `4` | Seasons retained in `state.json` |
| `PUBLISH_MEDIUM` | `true` | `false` publishes only high-confidence rows and sends medium to review |
| `MAX_CANDIDATES_PER_SUBJECT` | `40` | Cap on stored candidate observations |

## Season naming

Seasons follow the Japanese calendar quarters in **JST (UTC+9, no DST)**:

| Label | Months |
| --- | --- |
| `YYYY-winter` | January - March |
| `YYYY-spring` | April - June |
| `YYYY-summer` | July - September |
| `YYYY-fall` | October - December (alias `autumn` accepted when parsing) |

The current season is derived from the current date in JST, e.g. 2026-11-18 is
`2026-fall`.


## Published data

`/data/schedule/<season>.json` (and `latest.json`, plus `/data/history/`) is
served verbatim by the API. Field names are camelCase:

```json
{
  "schemaVersion": 1,
  "generatedAt": "2026-11-18T05:30:00Z",
  "season": "2026-fall",
  "seasonStart": "2026-10-01",
  "seasonEnd": "2026-12-31",
  "timezone": "Asia/Tokyo",
  "itemCount": 1,
  "items": [
    {
      "subjectId": 393010,
      "name": "探偵はもう、死んでいる。 Season 2",
      "nameCn": "侦探已经死了。 第二季"
      "airDate": "2026-10-07",
      "weekday": 3,
      "time": "23:00",
      "timezone": "Asia/Tokyo",
      "confidence": "high",
      "sources": [
        { "url": "https://example.com/schedule", "domain": "example.com", "provider": "brave" }
      ],
      "updatedAt": "2026-11-18T04:00:00Z"
    }
  ]
}
```

`weekday` is the **actual JST calendar day** (`1` = Monday ... `7` = Sunday) and
`time` is `HH:MM` JST. Late-night Japanese notation is normalised to the real
clock time: `鍦熸洔 25:15` and `鍦熸洔 娣卞1:15` both become **Sunday 01:15**.
Clients convert JST to the user's local time zone.

## Data quality & confidence

1. **Discovery**: Bangumi `POST /v0/search/subjects` (empty keyword + `air_date`
   range + `type=2`), falling back to `GET /calendar`. Only
   `id/name/name_cn/air_date/images/platform/infobox` are used.
2. **Evidence**: search snippets, fetched pages (rules first) and the subject's
   official website. The optional LLM fallback only runs for subjects that the
   rules could not resolve.
3. **Independence = source domain**. A snippet and a page from the same domain
   count once; `llm` counts as one source.
4. **Confidence**: two or more independent domains agreeing -> `high`; exactly
   one -> `medium`; disagreements or missing times -> excluded from the schedule
   and written to `/data/review.json` + `/data/review.md` (human readable).
   Set `PUBLISH_MEDIUM=false` to hold medium-confidence rows for review too.
5. **No data loss on transient failures**: a subject with no new evidence keeps
   its previous resolution (`carriedOver` in `state.json`); subjects outside the
   current `MAX_SUBJECTS_PER_RUN` window are untouched and rotate in on later
   runs (least-recently-checked first).
6. **Bounded state**: only the current and previous `STATE_RETENTION_SEASONS`
   seasons are kept in `state.json`; history is pruned to `HISTORY_KEEP`.

## Resource budget

| Resource | Target | How |
| --- | --- | --- |
| Runtime memory | 128 MiB Swarm limit (`docker-stack.yml`) | small Rust binary, 2 tokio workers, bounded concurrency (4 fetches), streams released after each run |
| Idle RSS | far below the limit | no browser/database; idle working set was ~10-20 MB in local verification |
| Disk | < 1 GB | cache 256 MB LRU, history <= 10 versions, state <= 4 seasons, logs on stdout (Docker json-file 10 MB x 3), image ~90 MB |
| Network | polite | 300 ms minimum interval, 15 s timeout, 200 KB/page, per-domain failure backoff, HTTP 429 pause |

## Deployment (Docker Swarm + Traefik + GitHub Actions)

The repository ships:

- `Dockerfile` — multi-stage, Debian bookworm slim, non-root uid 10001,
  `HEALTHCHECK` via `airradar --healthcheck`, `EXPOSE 8080`.
- `docker-stack.yml` — 1 replica, `restart_policy: any`, rolling update
  (`start-first`, rollback), healthcheck, 128 MB limit / 32 MB reservation,
  external `traefik-public` network, Traefik router labels, external docker
  secrets, named volume `/data`, json-file log rotation.
- `.github/workflows/docker.yml` — tests + build & push to
  `ghcr.io/moderras/airradar` (`latest` + `sha-<commit>`).
- `.github/workflows/deploy.yml` — build -> push GHCR -> SSH to the VPS ->
  create/rotate docker secrets -> `docker stack deploy --with-registry-auth`.

Full step-by-step instructions: [docs/deploy.md](docs/deploy.md).

### GitHub environment `production` - secrets

| Secret | Required | Purpose |
| --- | --- | --- |
| `SSH_HOST` | yes | VPS host/IP of the Swarm manager |
| `SSH_USER` | yes | SSH user with docker access (usually `root` or a docker-group user) |
| `SSH_KEY` | yes | Private SSH key (OpenSSH format) for that user |
| `SSH_PORT` | no | SSH port (default 22) |
| `GHCR_USER` | no | GHCR username for the remote `docker login` (default: repository owner) |
| `GHCR_TOKEN` | private image: yes | PAT with `read:packages`; only needed if the GHCR package is private |
| `TRAEFIK_HOST` | yes | Public hostname, e.g. `schedule.example.com` (used in the Traefik `Host()` rule) |
| `TRAEFIK_ENTRYPOINTS` | no | Traefik entrypoint (default `websecure`) |
| `TRAEFIK_CERTRESOLVER` | no | Traefik cert resolver (default `letsencrypt`) |
| `TRAEFIK_NETWORK` | no | External Traefik network (default `traefik-public`) |
| `ADMIN_TOKEN` | yes | Bearer token for `/admin/refresh` (long random string) |
| `BRAVE_API_KEY` | recommended | Brave Search key (at least one search provider is strongly recommended) |
| `TAVILY_API_KEY` | optional | Tavily Search key |
| `DEEPSEEK_API_KEY` | optional | DeepSeek search key |
| `LLM_API_KEY` | optional | LLM extraction fallback key |

### GitHub environment `production` - variables (optional)

| Variable | Default | Purpose |
| --- | --- | --- |
| `STACK_NAME` | `airradar` | Docker stack name |
| `DEPLOY_PATH` | `/opt/airradar` | Remote directory for `docker-stack.yml` + deploy script |
| `TRAEFIK_HOST` / `TRAEFIK_ENTRYPOINTS` / `TRAEFIK_CERTRESOLVER` / `TRAEFIK_NETWORK` | - | May be variables instead of secrets |
| `BANGUMI_UA` | AirRadar default UA | Bangumi User-Agent |
| `REFRESH_HOURS` | `6` | Collection interval |
| `MAX_SUBJECTS_PER_RUN` | `30` | Processing window |
| `MAX_SEARCH_CALLS` / `MAX_LLM_CALLS` | `60` / `10` | Per-run API budgets |
| `SEARCH_PROVIDER_ORDER` | `brave,tavily,deepseek` | Search fallback order |
| `INCLUDE_PLATFORMS` | `TV,WEB` | Bangumi platform filter |
| `SEASON_OVERRIDE` | empty | Force a season |
| `LLM_BASE_URL` / `LLM_MODEL` | empty | LLM fallback endpoint/model (key stays a secret) |
| `LOG_LEVEL` | `info` | Log verbosity |
| `CACHE_MAX_BYTES` / `HISTORY_KEEP` | `268435456` / `10` | Disk caps |

`docker-stack.yml` interpolates ${TRAEFIK_HOST}, ${TRAEFIK_ENTRYPOINTS},
${TRAEFIK_CERTRESOLVER} and ${TRAEFIK_NETWORK} at deploy time and
references five **external docker secrets**: `airradar_admin_token`,
`airradar_brave_api_key`, `airradar_tavily_api_key`,
`airradar_deepseek_api_key`, `airradar_llm_api_key`. `deploy-vps.sh` creates
missing secrets idempotently (optional keys get a whitespace placeholder that
AirRadar treats as unset).

## Rollback

- **GitHub Actions**: run the `deploy` workflow with
  `workflow_dispatch` -> `image_tag = sha-<previous commit>` (images are tagged
  `sha-<7 chars>` on every build; `latest` is a moving tag). The workflow skips
  the build and deploys the existing image.
- **Manual**: `SCHEDULE_IMAGE=ghcr.io/moderras/airradar:sha-<abc1234> bash
  scripts/deploy-vps.sh` on the VPS, or `docker service update --image ...`.
- Docker Swarm also keeps the previous task when a rolling update fails
  (`failure_action: rollback`), but the image tag points at `latest`, so prefer
  the explicit `image_tag` rollback.

## Development

```bash
cargo fmt --check          # formatting (optional locally)
cargo test                 # unit + HTTP integration tests (no network)
cargo build --release
cargo run -- --once --max-subjects 3   # real network: Bangumi + web pages
cargo run -- --probe-url https://example.com/page   # fetch one page, print slots
cargo run -- --healthcheck             # same probe Docker uses
```

Tests live next to the modules (extraction, verification, storage, cache,
config, season, provider parsers, scheduler) and in `tests/api.rs` (axum handlers
via `tower::oneshot`). Search/LLM/HTTP are covered by parser tests and fixtures;
no API keys or network access are needed for the test suite.

## License

MIT - see [LICENSE](LICENSE).
