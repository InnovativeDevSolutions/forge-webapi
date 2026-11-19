# forge-webapi

Standalone Web API for [Forge](https://github.com/InnovativeDevSolutions/forge), the Arma 3 persistent-server framework.

Forge's game-server persistence used to live in a Rust extension (`arma-rs`) that only runs while the Arma server is running — if the server crashed, unsaved session data could be lost. `forge-webapi` moves persistence out of the game process: a Rust HTTP API backed by SurrealDB, serving one or more Forge servers, so data survives a server crash and can be shared across servers.

## Architecture

One `docker compose` file per community:

```
Arma 3 server(s) ──webRequest──▶ forge-api (Rust, axum) ──▶ SurrealDB
                                    ▲ 8080 (public)
                                    │
                              SurrealDB 8000 (internal network only)
```

- **forge-api** — Rust API (`crates/forge-api`). Stateless; all durable state lives in SurrealDB.
- **SurrealDB** — the persistence store (namespace `forge`, database `main` by default). It is not exposed to the network — its port is bound to localhost only, for local debugging.
- **Web UI** — the existing Forge browser UIs (banking, garages, CAD, …) will talk to this API instead of the extension. Not yet wired up.

Each community runs its own stack — there is no shared hosted API.

## Quickstart

```sh
cp .env.example .env   # set a real SURREAL_PASSWORD
docker compose up --build
curl http://localhost:8080/health
# {"status":"ok","database":"connected"}
```

Or run the API directly (needs a SurrealDB on `SURREAL_ENDPOINT`):

```sh
export SURREAL_PASSWORD=...   # required, no default
cargo run -p forge-api
```

## Configuration

| Variable | Default | Description |
|---|---|---|
| `FORGE_API_BIND` | `0.0.0.0:8080` | Address the API listens on |
| `SURREAL_ENDPOINT` | `127.0.0.1:8000` | SurrealDB endpoint, `host:port` — no scheme (the HTTP engine adds it) |
| `SURREAL_NAMESPACE` | `forge` | SurrealDB namespace |
| `SURREAL_DATABASE` | `main` | SurrealDB database |
| `SURREAL_USER` | `root` | SurrealDB username |
| `SURREAL_PASSWORD` | *(required)* | SurrealDB password — no default |

**Security note:** the original Forge extension shipped `root`/`root` as SurrealDB defaults. This API deliberately has no password default — it refuses to start without `SURREAL_PASSWORD`. Set a long random password in `.env` (`.env` is gitignored; never commit it).

## Arma 3 / CORS

Arma 3's `webRequest` forces the `Origin` header to start with `arma://` (the `"origin"` key in the SQF request hashmap, e.g. `"forge"` → `Origin: arma://forge`) and sends a CORS preflight (`OPTIONS`) before every request. A wildcard `Access-Control-Allow-Origin: *` does not satisfy that, so the API echoes back the exact `arma://*` Origin it receives and answers preflights for `GET`/`POST`/`PUT`/`DELETE`/`OPTIONS`. See `crates/forge-api/src/cors.rs` and the example in `examples/sqf/webrequest_health.sqf`.

## CAD: groups, group status, contract availability

The CAD slice of the API covers exactly three things: groups, group
status, and which contracts are available vs not available. No
dispatching, units, incidents, or messaging.

Grounded in the original Forge implementation
(`lib/services/src/task.rs`, `arma/server/addons/cad/functions/fnc_initGroupRepository.sqf`,
`fnc_initCadStore.sqf`, and the CAD UI in `arma/client/addons/cad`):

- **Group status** is one of `available`, `en_route`, `on_task`,
  `holding`, `danger`, `unavailable` (default `available`; roles are
  `infantry`, `recon`, `armor`, `air`, `logistics`, `support`, default
  `infantry`). Anything else is rejected with `400 Invalid group status`.
- **Contract availability is derived, never stored**: a contract is
  available exactly when its status is `available`. Assigned, active,
  locked, succeeded and failed contracts are all unavailable.
- **Assignment** is part of availability state: contract responses
  expose `assignment` (`groupId`, `groupCallsign`, `assignedByUid`,
  `assignedByName`, `assignedAt`, `note`, `accepted`, `acceptedAt`) and
  `assignmentState` (`unassigned` / `assigned` / `accepted` / `active`).

### Contract lifecycle

```
available ──assign──▶ assigned ──accept──▶ active ──complete──▶ succeeded / failed
                         │  decline
                         └──────────▶ available (decline recorded)
```

- **Assign** (`POST /cad/contracts/:id/assign`, dispatcher): only an
  `available` contract can be assigned; sets the assignment fields
  (`groupId`, `groupCallsign`, `assignedByUid`, `assignedByName`,
  `assignedAt`, `note`) and moves it to `assigned`. Anything else is a
  `409 Task is not available (status: …)`.
- **Accept** (`POST /cad/contracts/:id/accept`, group leader): only the
  assigned group, only while `assigned` or `active`; binds ownership
  (`requesterUid`, `orgId`, `acceptedAt`) and moves it to `active`.
  Accepting twice is a `409 Task has already been accepted.`
- **Decline** (`POST /cad/contracts/:id/decline`, group leader): only the
  assigned group, only while `assigned` — an accepted (`active`)
  contract cannot be declined. Clears the assignment, records
  `lastDeclined` (group, callsign, who, when), and returns the contract
  to `available`.
- **Complete** (`POST /cad/contracts/:id/complete`): only `assigned` or
  `active` contracts, with `{"status": "succeeded"}` or `{"status":
  "failed"}`; records `completedAt` / `completedByUid` /
  `completedByName` (the original's completed-status storage). The
  assignment is kept, and the contract leaves the live board.

Request bodies carry the original's `TaskGroupContext` fields
(`groupId`, `groupCallsign`, `requesterUid`, `requesterName`, `orgId`,
`note`, `timestamp` in seconds; `timestamp` 0 means "now"). Mutation
responses are the updated contract plus a `message` (the original's
`TaskGroupMutationResult`). State violations answer `409` with the
original's wording. Mutations go through the dispatcher web UI / server
bridge as JSON — Arma's `webRequest` documents no request-body key, so
there is no SQF example for them (see `examples/sqf/webrequest_cad.sqf`
for the read calls).

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/cad/groups` | List groups (`?status=`, `?role=` filters) |
| `POST` | `/cad/groups` | Register / re-register a group (201 new, 200 existing) |
| `GET` | `/cad/groups/:id` | One group |
| `PUT` | `/cad/groups/:id/status` | Update a group's status (`{"status": "en_route"}`) |
| `GET` | `/cad/groups/:id/contracts` | Contracts assigned to one group |
| `GET` | `/cad/contracts` | List contracts (`?available=true/false`, `?status=`, `?group_id=`) |
| `GET` | `/cad/contracts/:id` | One contract with availability + assignment |
| `POST` | `/cad/contracts/:id/assign` | Assign an available contract to a group |
| `POST` | `/cad/contracts/:id/accept` | Assigned group accepts the contract |
| `POST` | `/cad/contracts/:id/decline` | Assigned group declines; contract back to available |
| `POST` | `/cad/contracts/:id/complete` | Complete an in-flight contract (`succeeded` / `failed`) |

Records live in SurrealDB tables `cad_group` (`group_id`, `callsign`,
`role`, `status`, `members` of `{uid, name}`, `org_id`, `updated_at`)
and `cad_contract` (`contract_id`, `title`, `type`, `priority`,
`status`, `locked`, `source`, `position`, `created_at`, `assignment`,
`accepted`, `requesterUid`, `orgId`, `lastDeclined`, `completedAt`,
`completedByUid`, `completedByName`). Contracts are seeded by the game
server bridge / mission, as the original's `upsert_catalog_entry` did.
Without a database connection the CAD routes answer `503 database
unavailable` (SurrealDB errors answer `502`); `/health` is unaffected.

## Roadmap

- [x] Skeleton: health endpoint, SurrealDB connection, `arma://` CORS, Docker
- [x] CAD: groups, group status, contract availability (+ assigned group)
- [x] Task / contract lifecycle: assign / accept / decline, completion
- [ ] Player data: bank, garages, lockers, phones, orgs, stores
- [ ] Auth for game servers and web UI clients
- [ ] Thin the Arma extension down to a bridge (or remove it) once the API owns persistence

## License

TBD — expected to match the Forge project license; to be confirmed.
