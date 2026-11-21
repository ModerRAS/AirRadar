# AirRadar HTTP API

Base URL: the public Traefik host chosen at deploy time (for example
`https://schedule.example.com`). The container itself listens on
`0.0.0.0:8080`.

All responses are JSON. Times are ISO-8601 UTC unless the field says otherwise;
schedule rows are always **JST (UTC+9, no DST)**.

## Conventions

| Field | Meaning |
| --- | --- |
| `schemaVersion` | Payload schema version, currently `1`. Bump on breaking changes only. |
| `generatedAt` | When the schedule was published (UTC, RFC3339). |
| `weekday` | Actual JST calendar day: `1` = Monday ... `7` = Sunday. |
| `time` | `HH:MM` in JST, 24-hour clock. Late-night Japanese TV notation is normalised: `土曜 25:15` becomes Sunday `01:15`. |
| `season` | `YYYY-winter`, `YYYY-spring`, `YYYY-summer`, `YYYY-fall` (JST calendar quarters). |
| `confidence` | `high` (>= 2 independent domains agree), `medium` (1 source) or `low` (reserved, review only). |
| `sources` | Independent domains behind the row (same-domain pages count once). |

Error payload:

```json
{ "error": "invalid_season", "message": "invalid season \"2026-Q4\"; expected YYYY-season (for example 2026-fall)" }
```

Common error codes: `no_schedule_data`, `season_not_found`,
`missing_season`, `invalid_season`, `unauthorized`,
`admin_disabled`, `refresh_in_progress`, `not_found`,
`internal_error`.

## GET /health

Liveness probe used by Docker and Traefik. No auth, no disk access.

```bash
curl -fsS https://schedule.example.com/health
```

```json
{
  "status": "ok",
  "service": "airradar",
  "version": "0.1.0",
  "schemaVersion": 1,
  "uptimeSeconds": 3210,
  "time": "2026-11-18T05:30:00Z"
}
```

Returns HTTP 200 whenever the process is serving. Use it for container
healthchecks; a 200 does not guarantee that a collection has succeeded (check
`/api/status` for that).

## GET /api/schedule/latest

Returns the most recently published schedule document (any season).

```bash
curl -fsS https://schedule.example.com/api/schedule/latest
```

```json
{
  "schemaVersion": 1,
  "generatedAt": "2026-11-18T05:15:00Z",
  "season": "2026-fall",
  "seasonStart": "2026-09-21",
  "seasonEnd": "2026-12-31",
  "timezone": "Asia/Tokyo",
  "itemCount": 2,
  "items": [
    {
      "subjectId": 393010,
      "name": "探偵はもう、死んでいる。 Season 2",
      "nameCn": "侦探已经死了。 第二季",
      "airDate": "2026-10-07",
      "image": "https://lain.bgm.tv/pic/cover/l/65/61/393010_EIOi9.jpg",
      "weekday": 3,
      "time": "23:00",
      "timeKnown": true,
      "timezone": "Asia/Tokyo",
      "confidence": "high",
      "sources": [
        { "url": "https://example.com/schedule", "domain": "example.com", "provider": "brave" },
        { "url": "https://tanmoshi-anime.jp/Season2/", "domain": "tanmoshi-anime.jp", "provider": "official_site" }
      ],
      "updatedAt": "2026-11-18T04:00:00Z"
    }
  ]
}
```

- HTTP 404 `{ "error": "no_schedule_data" }` before the first successful publish.
- `seasonStart` is the **broadcast window start**: the calendar season start
  minus `SEASON_PADDING_DAYS` (default 10). For fall 2026 this is
  **2026-09-21**, so late-September premieres are included; 2026-09-20 is
  outside the window. `seasonEnd` stays the calendar season end.
- `items` only contains rows that passed verification; conflicts and missing
  times are in `/data/review.json` / `review.md` on the server.
- Rows keep their previous resolution when a later collection finds no new
  evidence (transient source failure), so the list does not flap.

## GET /api/schedule?season=YYYY-season

Returns the archived published document for one season.

```bash
curl -fsS 'https://schedule.example.com/api/schedule?season=2026-fall'
```

| Status | Meaning |
| --- | --- |
| 200 | Schedule document for that season |
| 400 `missing_season` | `season` query parameter missing |
| 400 `invalid_season` | Not `YYYY-season` (for example `2026-Q4`) |
| 404 `season_not_found` | No schedule published for that (valid) season yet |

## GET /api/status

Operational snapshot: last run, next planned run, search providers, LLM config,
cache usage and data version. Useful for dashboards and debugging.

```bash
curl -fsS https://schedule.example.com/api/status
```

```json
{
  "service": "airradar",
  "version": "0.1.0",
  "schemaVersion": 1,
  "uptimeSeconds": 3210,
  "time": "2026-11-18T05:30:00Z",
  "running": false,
  "refreshHours": 6.0,
  "seasonOverride": null,
  "nextScheduledAt": "2026-11-18T11:24:00Z",
  "data": {
    "latestSeason": "2026-fall",
    "latestGeneratedAt": "2026-11-18T05:15:00Z",
    "latestItemCount": 2,
    "historyVersions": 4
  },
  "lastRun": {
    "runId": "20261118T051500Z-6b7d8afb",
    "trigger": "schedule",
    "season": "2026-fall",
    "startedAt": "2026-11-18T05:14:58Z",
    "finishedAt": "2026-11-18T05:15:03Z",
    "durationMs": 5120,
    "ok": true,
    "subjectListSource": "bangumi-search",
    "subjectsFetched": 172,
    "subjectsProcessed": 120,
    "itemsPublished": 96,
    "weekdayOnlyCount": 51,
    "highConfidence": 28,
    "mediumConfidence": 17,
    "reviewCount": 3,
    "unknownCount": 0,
    "search": [
      {
        "provider": "brave",
        "attempts": 34,
        "queries": 34,
        "results": 212,
        "elapsedMs": 8450,
        "failures": 0
      }
    ],
    "llm": { "configured": false, "calls": 0, "cachedHits": 0, "results": 0, "failures": 0 },
    "fetch": {
      "requested": 231,
      "fetched": 104,
      "fromCache": 5,
      "failed": 122,
      "retried": 118,
      "failuresTimeout": 27,
      "failuresHttp": 91,
      "failuresOther": 4,
      "failureCategories": [
        {"category": "http_status", "count": 91, "exampleDomain": "example.jp", "exampleReason": "http status 403"},
        {"category": "unsupported_content_type", "count": 4, "exampleDomain": "example.com", "exampleReason": "unsupported content type: image/png"}
      ],
      "topFailures": [
        {"domain": "example.jp", "category": "http_status", "status": 403, "reason": "http status 403"}
      ]
    },
    "deepseekFetch": {
      "configured": true,
      "calls": 68,
      "successes": 61,
      "failures": 7,
      "rateLimited": 1,
      "budgetExhausted": 0
    },
    "errors": []
  },
  "search": {
    "providers": ["brave"],
    "configuredOrder": ["brave", "tavily", "deepseek"]
  },
  "llm": { "configured": false },
  "cache": { "entries": 118, "bytes": 18342122, "maxBytes": 268435456 },
  "limits": {
    "maxSubjectsPerRun": 120,
    "maxLlmCalls": 40,
    "fetchConcurrency": 4,
    "historyKeep": 10,
    "cacheMaxBytes": 268435456
  }
}
```

Notes:

- `lastRun` is persisted in `/data/status.json` and therefore survives restarts.
- `nextScheduledAt` is computed by the in-process scheduler (in-memory) and
  is `null` for a second after a restart until the scheduler loop runs.
- `search.providers` lists only providers that have a key configured;
  `search.configuredOrder` is the configured fallback order.
- `lastRun.fetch` counts local page fetches: `retried` is the number of extra
  attempts made by the one-shot transient retry; `failuresTimeout`/`failuresHttp`/
  `failuresOther` classify failures; `failureCategories` aggregates detailed
  categories; `topFailures` samples up to five failures as **domain + reason**
  (never a full URL with a path/query).
- `lastRun.deepseekFetch` reports the server-side DeepSeek Tools Fetch fallback:
  `calls`, `successes`, `failures`, `rateLimited` (HTTP 429/cooldown) and
  `budgetExhausted` (per-run `MAX_DEEPSEEK_FETCH_CALLS`).
- `subjectsFetched` is the number of Bangumi subjects that survived the
  `INCLUDE_PLATFORMS` filter (the platform-eligible set), and
  `subjectsProcessed` is the selected window. With the default
  `MAX_SUBJECTS_PER_RUN=120` and a typical season this is a full-season single
  run, so `subjectsProcessed` should be close to `subjectsFetched`.
- `running` is true while a collection is in progress.

## POST /admin/refresh

Triggers one background collection immediately. Requires:

```http
Authorization: Bearer <ADMIN_TOKEN>
```

Optional query parameter `season=YYYY-season` forces a season for this run
(otherwise `SEASON_OVERRIDE` or the current JST season is used).

```bash
curl -fsS -X POST \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  'https://schedule.example.com/admin/refresh?season=2026-fall'
```

```json
{ "accepted": true, "runId": "20261118T053000Z-1a2b3c4d", "season": "2026-fall", "trigger": "manual" }
```

| Status | Condition |
| --- | --- |
| 202 | Run accepted and started in the background |
| 400 `invalid_season` | `season` is not `YYYY-season` |
| 401 `unauthorized` | Header missing or token mismatch |
| 409 `refresh_in_progress` | Another run (scheduled or manual) is active |
| 503 `admin_disabled` | `ADMIN_TOKEN` is not configured on the server |

Only one collection runs at a time. The endpoint returns immediately; poll
`/api/status` (`running`, then `lastRun` / `data.latestGeneratedAt`)
to follow progress.

## Server data layout (for operators)

| Path | Content |
| --- | --- |
| `/data/schedule/latest.json` | Last published schedule (served by `/api/schedule/latest`) |
| `/data/schedule/<season>.json` | Per-season published schedule |
| `/data/history/<season>-<timestamp>.json` | Retained published versions (`HISTORY_KEEP`) |
| `/data/state.json` | Latest candidate observations, per-subject confidence and first/last seen timestamps |
| `/data/status.json` | Last run summary (served inside `/api/status`) |
| `/data/review.json`, `/data/review.md` | Review queue: conflicts + missing times, with evidence |
| `/data/llm-cache.json` | LLM fallback result cache (only when `LLM_*` configured) |
| `/data/llm-session-id` | Stable OpenCode Go session id (auto-generated when `LLM_SESSION_ID` is empty) |
| `/data/cache/` | Fetched page cache (256 MB LRU by default) |

All JSON writes go through a temporary file + fsync + atomic rename, so readers
never observe partial documents.
