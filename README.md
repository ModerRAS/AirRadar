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
Bangumi API ──▶ subject list (season, id/name/name_cn/air_date/images)
      ├─▶ search providers (Brave / Tavily / DeepSeek, ordered fallback)
      ├─▶ snippets + candidate URLs
      ├─▶ HTTP fetch (rate limited, 4 concurrent, 15 s timeout, 200 KB cap, disk cache)
      ├─▶ HTML -> plain text
      ├─▶ rule extraction (JP/ZH/EN patterns) ──▶ optional LLM fallback (batched)
      ├─▶ cross-source verification (independence = domain) ──▶ confidence + review queue
      └─▶ /data/state.json, /data/schedule/<season>.json, /data/review.{json,md}
```

The service is a **resident web service** (Swarm service with
restart_policy: any + healthcheck). The collector is **not a separate daemon**:
the same process runs a tokio interval timer (`REFRESH_HOURS`, jitter ±10%),
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
| `SEASON_PADDING_DAYS` | `10` | Broadcast window starts this many days before the calendar season start (fall starts Sep 21) |
| `REFRESH_HOURS` | `6` | Collection interval (fractional allowed) |
| `MAX_SUBJECTS_PER_RUN` | `120` | Processing window per run; a typical season is covered in one run (still rotates by last-checked) |
| `INCLUDE_PLATFORMS` | `TV,WEB` | Bangumi platforms to include (`ALL` = no filter) |
| `SEARCH_PROVIDER_ORDER` | `brave,tavily,deepseek` | Search backends in fallback order; only providers with a key are enabled |
| `SEARCH_QUERIES_PER_SUBJECT` | `2` | Search queries issued per subject |
| `MAX_SEARCH_CALLS` | `400` | Search-query budget per run |
| `SEARCH_FALLBACK_ON_EMPTY` | `false` | Also try the next provider when a provider returns zero results |
| `SEARCH_TIMEOUT_SECS` | `20` | Per search request timeout |
| `BRAVE_API_KEY` / `_FILE` | unset | Brave Search API key |
| `TAVILY_API_KEY` / `_FILE` | unset | Tavily Search API key |
| `DEEPSEEK_API_KEY` / `_FILE` | unset | DeepSeek **search** key (independent from the OpenCode Go LLM key) |
| `DEEPSEEK_SEARCH_URL` | `https://api.deepseek.com/v1/tools/search` | Official DeepSeek server-side Search API (`/chat/completions` keeps the legacy prompt search) |
| `DEEPSEEK_SEARCH_COUNT` | `10` | Results per DeepSeek search query (1-50) |
| `DEEPSEEK_FETCH_URL` | `https://api.deepseek.com/v1/tools/fetch` | Official DeepSeek server-side Fetch API used as a local-fetch fallback |
| `MAX_DEEPSEEK_FETCH_CALLS` | `150` | Per-run budget for server-side fetch fallback calls |
| `DEEPSEEK_SEARCH_MODEL` | `deepseek-chat` | Only for legacy `/chat/completions` prompt search |
| `LLM_BASE_URL` | unset | OpenAI-compatible extraction fallback base URL |
| `LLM_API_KEY` / `_FILE` | unset | Extraction fallback key (OpenCode Go subscription key; independent from search keys) |
| `LLM_MODEL` | unset | Extraction fallback model (all three of LLM_BASE_URL/LLM_API_KEY/LLM_MODEL must be set) |
| `LLM_SESSION_ID` | empty | OpenCode Go session id; when empty a stable id is generated and persisted in `DATA_DIR/llm-session-id` |
| `LLM_BATCH_SIZE` | `8` | Subjects per LLM request |
| `MAX_LLM_CALLS` | `40` | LLM request budget per run |
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
| `PUBLISH_WEEKDAY_ONLY` | `true` | Publish weekday-only rows (`time: null`, `timeKnown: false`, confidence `unknown`) for subjects without a verified time |
| `MAX_CANDIDATES_PER_SUBJECT` | `40` | Cap on stored candidate observations |

### Credentials: search vs LLM (independent)

AirRadar uses two independent credential sets and never mixes them:

| Purpose | Variables |
| --- | --- |
| Web search / discovery (snippets, URLs, pages) | `SEARCH_PROVIDER_ORDER`, `DEEPSEEK_API_KEY` (+ `DEEPSEEK_SEARCH_URL`, `DEEPSEEK_SEARCH_COUNT`, and the server-side fetch fallback `DEEPSEEK_FETCH_URL`/`MAX_DEEPSEEK_FETCH_CALLS`), `BRAVE_API_KEY`, `TAVILY_API_KEY` |
| LLM extraction fallback (rules failed) | `LLM_BASE_URL`, `LLM_API_KEY`, `LLM_MODEL`, `LLM_SESSION_ID` |

Example setup with DeepSeek search + server-side fetch + OpenCode Go LLM:

```bash
# search: its own DeepSeek key; official server-side Search/Fetch APIs
SEARCH_PROVIDER_ORDER=deepseek
DEEPSEEK_API_KEY=<DeepSeek API key>
DEEPSEEK_SEARCH_URL=https://api.deepseek.com/v1/tools/search
DEEPSEEK_SEARCH_COUNT=10
DEEPSEEK_FETCH_URL=https://api.deepseek.com/v1/tools/fetch
MAX_DEEPSEEK_FETCH_CALLS=150

# LLM fallback: OpenCode Go subscription (never the DeepSeek search key)
LLM_BASE_URL=https://opencode.ai/inference/go/v1
LLM_API_KEY=<OpenCode Go subscription key>
LLM_MODEL=<OpenCode Go model id>
LLM_SESSION_ID=                     # optional; auto-generated when empty
```

The search router only reads the search keys; the LLM client only reads
`LLM_*`. Setting one group never enables the other.

### DeepSeek official Search / Fetch APIs

DeepSeek search uses the official server-side API by default:

- `POST https://api.deepseek.com/v1/tools/search` with `{query, count, need_content, ori_question}`;
  results provide real `title`/`url` plus `summary`/`content`, which are used as evidence.
- `POST https://api.deepseek.com/v1/tools/fetch` with `{url}` fetches a page server-side
  (title + Markdown text). AirRadar only calls it when a local page fetch failed
  (anti-bot/TLS/region/empty JS shell), bounded by `MAX_DEEPSEEK_FETCH_CALLS`
  (default 150/run) with 5 QPS-friendly throttling and 429 cooldown.
- Both endpoints share `DEEPSEEK_API_KEY`; neither uses `LLM_API_KEY`.
- A URL containing `/chat/completions` keeps the legacy prompt-based search for
  backwards compatibility; the default is the official tools API.

### OpenCode Go (LLM extraction fallback)

- Base URL: `https://opencode.ai/inference/go/v1` (OpenAI-compatible
  `/chat/completions`; the Anthropic-style base is not used by AirRadar).
- Auth: `Authorization: Bearer $LLM_API_KEY`.
- Session header: the OpenCode Go gateway **requires**
  `x-opencode-session: <sessionId>` on every request and answers HTTP 400
  without it. AirRadar adds the header automatically when the endpoint host
  contains `opencode.ai` (other OpenAI-compatible endpoints do not get it).
- Session id resolution: explicit `LLM_SESSION_ID` → persisted
  `DATA_DIR/llm-session-id` → generated once and written atomically. The id is
  stable across restarts and redeploys because `/data` is a Docker volume.
  It is a routing id, not a secret, and is never printed with a key.
- Enable the fallback with all three of `LLM_BASE_URL`/`LLM_API_KEY`/`LLM_MODEL`;
  `MAX_LLM_CALLS` and `LLM_BATCH_SIZE` bound cost.

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

**Broadcast window.** Anime premieres often start a few days before the
calendar quarter (fall shows start in late September). AirRadar queries
`[season.start() - SEASON_PADDING_DAYS, season.end_exclusive())`, so with the
default 10 days the 2026-fall window starts on **2026-09-21** (2026-09-20 is
excluded). The published `seasonStart` field reports this broadcast-window
start, not the calendar quarter start. The same padding applies symmetrically
to winter/spring/summer windows and to the /calendar fallback filter.


## Published data

`/data/schedule/<season>.json` (and `latest.json`, plus `/data/history/`) is
served verbatim by the API. Field names are camelCase:

```json
{
  "schemaVersion": 1,
  "generatedAt": "2026-11-18T05:30:00Z",
  "season": "2026-fall",
  "seasonStart": "2026-09-21",
  "seasonEnd": "2026-12-31",
  "timezone": "Asia/Tokyo",
  "itemCount": 1,
  "items": [
    {
      "subjectId": 393010,
      "name": "探偵はもう、死んでいる。 Season 2",
      "nameCn": "侦探已经死了。 第二季",
      "airDate": "2026-10-07",
      "weekday": 3,
      "time": "23:00",
      "timeKnown": true,
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
`time` is `HH:MM` JST when known. Late-night Japanese notation is normalised to
the real clock time: `土曜 25:15` and `土曜 深夜1:15` both become **Sunday 01:15**.
Clients convert JST to the user's local time zone.

Weekday-only rows (Bangumi has a weekday but no verified clock time) are
published with `"time": null`, `"timeKnown": false` and
`"confidence": "unknown"` when `PUBLISH_WEEKDAY_ONLY=true` (default). The
`timeKnown` field defaults to `true` when missing, so older payloads stay
compatible; clients should render such rows as "time unknown" under the
weekday.

## Data quality & confidence

1. **Discovery**: Bangumi `POST /v0/search/subjects` (empty keyword + `air_date`
   range + `type=2`), falling back to `GET /calendar`. Only
   `id/name/name_cn/air_date/images/platform/infobox` are used.
2. **Evidence**: DeepSeek official search results (real URLs + summary/content),
   search result titles + snippets (extracted independently of page fetches, so
   a failed fetch can still yield a time), locally fetched pages, the subject's
   official website, and — when a local fetch fails — the DeepSeek server-side
   fetch fallback. Transient HTTP failures get one retry; failure categories and
   sampled domains are reported in `/api/status.lastRun.fetch`. The optional LLM
   fallback only runs for subjects that the rules could not resolve.
3. **Independence = source domain**. A snippet and a page from the same domain
   count once; `llm` counts as one source.
4. **Confidence**: two or more independent domains agreeing -> `high`; exactly
   one -> `medium`; conflicts -> excluded from the schedule and written to
   `/data/review.json` + `/data/review.md` (human readable). Subjects with no
   verified time are published as weekday-only rows with confidence `unknown`
   when `PUBLISH_WEEKDAY_ONLY=true` (default), so the whole season is listed;
   set it to `false` to keep them in the review queue instead.
   Set `PUBLISH_MEDIUM=false` to hold medium-confidence rows for review too.
5. **Coverage**: the Bangumi search is paginated until the full broadcast
   window is collected (the API returns ~20 items per page; earlier versions
   stopped after 4 pages, capping runs at ~80 subjects). The default
   `MAX_SUBJECTS_PER_RUN=120` then selects the whole season in one run
   (min(platform-eligible season size, 120)); if a season is larger, the
   remaining subjects rotate in on later runs (least-recently-checked first).
   A subject with no new evidence keeps its previous resolution (`carriedOver`
   in `state.json`).
6. **Bounded state**: only the current and previous `STATE_RETENTION_SEASONS`
   seasons are kept in `state.json`; history is pruned to `HISTORY_KEEP`.

## Resource budget

| Resource | Target | How |
| --- | --- | --- |
| Runtime memory | 128 MiB Swarm limit (`docker-stack.yml`) | small Rust binary, 2 tokio workers, bounded concurrency (4 fetches), streams released after each run |
| Idle RSS | far below the limit | no browser/database; idle working set was ~10-20 MB in local verification |
| Disk | < 1 GB | cache 256 MB LRU, history <= 10 versions, state <= 4 seasons, logs on stdout (Docker json-file 10 MB x 3), image ~90 MB |
| Network | polite | 300 ms minimum interval, 15 s timeout, 200 KB/page, per-domain failure backoff, HTTP 429 pause, one retry for transient errors |

The larger default processing window does not raise memory: page fetch
concurrency stays capped by `FETCH_CONCURRENCY` (4) and the page cache is
bounded by `CACHE_MAX_BYTES` (256 MB), so a full-season run still fits the
128 MB Swarm limit.

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
  It also passes `LLM_SESSION_ID`, `LLM_BATCH_SIZE`, `PUBLISH_WEEKDAY_ONLY` and the
  DeepSeek tools settings (`DEEPSEEK_SEARCH_URL`, `DEEPSEEK_SEARCH_COUNT`,
  `DEEPSEEK_FETCH_URL`, `MAX_DEEPSEEK_FETCH_CALLS`) when configured (search and
  LLM credentials stay independent).

Full step-by-step instructions: [docs/deploy.md](docs/deploy.md).

### GitHub environment `production` - secrets

| Secret | Required | Purpose |
| --- | --- | --- |
| `SSH_HOST` | yes | VPS host/IP of the Swarm manager |
| `SSH_USER` | yes | SSH user with docker access (usually `root` or a docker-group user) |
| `SSH_KEY` | yes | Private SSH key (OpenSSH format) for that user |
| `SSH_PORT` | no | SSH port. **Must be set if your SSH port is not 22** (default 22) |
| `GHCR_USER` | no | GHCR username for the remote `docker login` (default: repository owner) |
| `GHCR_TOKEN` | private image: yes | PAT with `read:packages`; only needed if the GHCR package is private |
| `TRAEFIK_HOST` | yes | Public hostname, e.g. `schedule.example.com` (used in the Traefik `Host()` rule) |
| `TRAEFIK_ENTRYPOINTS` | no | Traefik entrypoint (default `websecure`) |
| `TRAEFIK_CERTRESOLVER` | no | Traefik cert resolver (default `letsencrypt`) |
| `TRAEFIK_NETWORK` | no | External Traefik network (default `traefik-public`) |
| `ADMIN_TOKEN` | yes | Bearer token for `/admin/refresh` (long random string) |
| `BRAVE_API_KEY` | recommended | Brave Search key (at least one search provider is strongly recommended) |
| `TAVILY_API_KEY` | optional | Tavily Search key |
| `DEEPSEEK_API_KEY` | optional | DeepSeek **search** key (independent from the OpenCode Go LLM key) |
| `LLM_API_KEY` | optional | OpenCode Go / OpenAI-compatible extraction key (independent from search keys) |

### GitHub environment `production` - variables (optional)

| Variable | Default | Purpose |
| --- | --- | --- |
| `STACK_NAME` | `airradar` | Docker stack name |
| `DEPLOY_PATH` | `/opt/airradar` | Remote directory for `docker-stack.yml` + deploy script |
| `TRAEFIK_HOST` / `TRAEFIK_ENTRYPOINTS` / `TRAEFIK_CERTRESOLVER` / `TRAEFIK_NETWORK` | - | May be variables instead of secrets |
| `BANGUMI_UA` | AirRadar default UA | Bangumi User-Agent |
| `REFRESH_HOURS` | `6` | Collection interval |
| `MAX_SUBJECTS_PER_RUN` | `120` | Processing window (full season in one run) |
| `MAX_SEARCH_CALLS` / `MAX_LLM_CALLS` | `400` / `40` | Per-run API budgets |
| `SEASON_PADDING_DAYS` | `10` | Broadcast window padding before the calendar season |
| `PUBLISH_WEEKDAY_ONLY` | `true` | Publish weekday-only (time unknown) rows |
| `LLM_BATCH_SIZE` | `8` | Subjects per LLM request |
| `SEARCH_PROVIDER_ORDER` | `brave,tavily,deepseek` | Search fallback order (for DeepSeek-only search use `deepseek`) |
| `DEEPSEEK_SEARCH_URL` / `DEEPSEEK_SEARCH_COUNT` | official tools defaults | DeepSeek server-side search endpoint and result count |
| `DEEPSEEK_FETCH_URL` / `MAX_DEEPSEEK_FETCH_CALLS` | official tools defaults / `150` | DeepSeek server-side fetch fallback endpoint and per-run budget |
| `DEEPSEEK_SEARCH_MODEL` | `deepseek-chat` | Only for legacy `/chat/completions` search |
| `INCLUDE_PLATFORMS` | `TV,WEB` | Bangumi platform filter |
| `SEASON_OVERRIDE` | empty | Force a season |
| `LLM_BASE_URL` / `LLM_MODEL` | empty | LLM fallback endpoint/model, e.g. OpenCode Go base (key stays a secret) |
| `LLM_SESSION_ID` | empty | Optional OpenCode Go session id; auto-generated and persisted when empty |
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
