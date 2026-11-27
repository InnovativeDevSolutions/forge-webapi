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
- **SurrealDB** — the persistence store (namespace `forge`, database `main` by default). It is not exposed to the network — its port is bound to localhost only, for local debugging. On startup the API defines its tables (`DEFINE TABLE IF NOT EXISTS ... SCHEMALESS`, see `crates/forge-api/src/db.rs`), so a fresh database works on first boot with no manual migration step; the step is idempotent and leaves existing data alone.
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
| `POST` | `/cad/contracts` | Seed one contract into the catalog (409 if the id exists) |
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

## Player data: bank, garages, lockers, phones, orgs, stores

The persistent player state the original extension held in memory:
bank accounts, garages (stored vehicles + the virtual garage of
unlocked vehicle classnames), lockers (stored items + the virtual
locker of unlocked equipment classnames), phones (contacts, messages,
emails), organizations, and the store checkout that ties them
together. Grounded in `lib/services/src/bank.rs`, `garage.rs`,
`locker.rs`, `phone.rs`, `org.rs`, `store.rs`, `v_garage.rs`,
`v_locker.rs` and the matching models in `lib/models/src/`.

**Auth: these endpoints are unauthenticated for now**, exactly like
the CAD and lifecycle slices. Auth (player login screen in-game, the
API issuing a token, player endpoints only answering with a valid
token) was parked by Jacob as a to-do for later — do not mistake the
absence of auth here for a decision that it is unnecessary: any caller
can currently read and mutate any player's bank, garage, locker,
phone and org data.

- **Bank** (`player_bank`: `uid`, `name`, `bank`, `cash`, `earnings`,
  `pin`, `transactions`). Money is `f64`, as the original stores it;
  the PIN is a four-digit number (1000–9999). Deposit moves cash into
  the bank, withdraw the reverse; earnings deposits move pending
  earnings into the bank. `PUT /players/{uid}/bank` replaces the whole
  account (the hot-state save); `DELETE` removes it. The original distinguishes the full bank
  interface (`mode: "bank"`, the web UI) from an ATM session (`mode:
  "atm"`): deposits/withdrawals from an ATM need `atmAuthorized`,
  earnings deposits, transfers and PIN changes are bank-interface
  only, PIN validation is ATM only. `mode` defaults to `"bank"`.
  Transfers pay from the source's `cash` or `bank` (`fromField`,
  default `bank`) into the target's `bank`.
- **Garage** (`player_garage`: `uid`, `vehicles` keyed by plate).
  At most **5 vehicles**; a stored vehicle's plate is a server-side
  UUID v4, as the original generates it. `fuel`/`damage` are 0.0–1.0
  and hit-point arrays (`names`, `selections`, `values`) must match
  in length with values 0.0–1.0. **Virtual garage**
  (`player_vgarage`: `uid`, `cars`, `armor`, `helis`, `planes`,
  `naval`, `other`) holds the unlocked vehicle classnames; adding one
  that is already unlocked is a no-op.
- **Locker** (`player_locker`: `uid`, `items` keyed by classname).
  At most **25 unique items**; storing a classname that is already
  there overwrites it. Items are `{category, classname, amount}` with
  `amount >= 1`. **Virtual locker** (`player_vlocker`: `uid`,
  `items`, `weapons`, `magazines`, `backpacks`) holds the unlocked
  equipment classnames.
- **Phone** (`player_phone`: `uid`, `contacts`, `messages`, `emails`).
  Init seeds contacts with the player themself and the Field
  Commander (`field_commander`). Messages and emails are indexed on
  both sides (once for a self-message/email); the Field Commander
  cannot *receive* player messages or emails but can send them.
  Message ids are `phone-message:{from}:{to}:{sequence}`, email ids
  `phone-email:{from}:{to}:{sequence}`; an empty email subject
  becomes `"No subject"`. Deleting removes only the requesting
  player's copy. Sends accept a client-supplied `id` (the extension
  pre-assigns ids) and are idempotent per phone — sending the same id
  twice does not duplicate it. `PUT /players/{uid}/phone` replaces
  the whole payload (the bridge's migration seed); `DELETE` removes it.
- **Actor** (`player_actor`: `uid`, `name`, `loadout`, `position`,
  `direction`, `stance`, `email`, `phoneNumber`, `state`, `holster`,
  `rank`, `organization`). A separate table from the legacy `actor`
  table on purpose: the legacy record is snake_case (`phone_number`)
  and sharing the table would drop the phone number on read. UIDs are
  numeric, 12–17 digits (the relaxed single-player rule).
- **Orgs** (`org`: `orgId`, `owner`, `name`, `funds`, `reputation`,
  `creditLines`, `assets`, `fleet`, `members`, `pendingInvites`).
  Every player starts in the **`default`** org (auto-created on first
  use, owner `"server"`); registering moves the requester out of it,
  leaving or disbanding moves members back into it. Treasury actions
  (invites, credit lines, org-funds charges) are for the org owner,
  or — for the default/server-owned orgs — a caller flagged
  `requesterIsDefaultOrgCeo`. Org ids are alphanumeric plus
  underscores. Credit lines carry the original's 10% default interest
  rate and are rounded to cents; assigning a line reserves the amount
  from org funds, repaying splits principal and interest
  proportionally. Fleet vehicles are keyed `{classname}_{index}` with
  status `"Ready"`, damage `"0%"`.
- **Store** (`POST /store/checkout`). The original has **no store
  catalog and no sell operation**: the caller sends the cart as seeds
  that already carry their price (`priceValue`), and checkout only
  buys. Items land in the player's locker and unlock in the virtual
  locker; vehicles unlock in the virtual garage; units are only
  *reported* as granted (the original has no unit storage). Payment
  is `cash` / `bank` / `org_funds` (leader/CEO only; vehicles bought
  this way also join the org fleet) / `credit_line`. The locker may
  not grow past 25 unique items. Commit order is locker → virtual
  locker → virtual garage → org → bank, with the original's
  best-effort rollback of what was already written on failure.

### Reward hook

After `POST /cad/contracts/{id}/complete` with `succeeded`, the
bridge reads the contract's `requesterUid` (and `orgId`) and credits
each participant's pending earnings with `POST
/players/{uid}/bank/credit-earnings` — mirroring the original's task
reward service (`fnc_TaskRewardService.sqf`, `applyRatingOutcome`),
which adds each participant's share of the rating delta to
`earnings` via a BankStore `mset` patch. The player then moves
earnings into the bank with `deposit-earnings`. Org reputation is
adjusted the same way with `POST /orgs/{id}/reputation` (clamped at
zero).

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/players/{uid}/bank` | Create a bank account (`name`, `pin`; 409 if it exists) |
| `GET` | `/players/{uid}/bank` | One account |
| `PUT` | `/players/{uid}/bank` | Replace the whole account (hot-state save; creates if missing) |
| `DELETE` | `/players/{uid}/bank` | Remove an account |
| `POST` | `/players/{uid}/bank/deposit` | Move cash into the bank (`amount`, `mode`, `atmAuthorized`) |
| `POST` | `/players/{uid}/bank/withdraw` | Move bank balance into cash |
| `POST` | `/players/{uid}/bank/deposit-earnings` | Move pending earnings into the bank (bank interface only) |
| `POST` | `/players/{uid}/bank/credit-earnings` | Credit pending earnings — the contract-reward hook |
| `POST` | `/players/{uid}/bank/transfer` | Transfer to another player's bank (`targetUid`, `fromField`) |
| `POST` | `/players/{uid}/bank/validate-pin` | Check a PIN (ATM only; 401 on a wrong PIN) |
| `POST` | `/players/{uid}/bank/change-pin` | Change a PIN (bank interface only) |
| `POST` | `/players/{uid}/bank/charge` | Charge cash/bank for a checkout (`sourceField`, `commit`) |
| `POST` | `/players/{uid}/garage` | Create an empty garage (409 if it exists) |
| `GET` | `/players/{uid}/garage` | One player's garage |
| `PUT` | `/players/{uid}/garage` | Replace the whole garage (bulk sync, max 5 vehicles) |
| `DELETE` | `/players/{uid}/garage` | Remove a whole garage |
| `POST` | `/players/{uid}/garage/vehicles` | Store a vehicle (plate is a fresh UUID v4) |
| `PATCH` | `/players/{uid}/garage/vehicles/{plate}` | Patch damage / fuel / hit points |
| `DELETE` | `/players/{uid}/garage/vehicles/{plate}` | Remove a stored vehicle |
| `POST` | `/players/{uid}/vgarage` | Create an empty virtual garage |
| `GET` | `/players/{uid}/vgarage` | Unlocked vehicle classnames |
| `DELETE` | `/players/{uid}/vgarage` | Remove a whole virtual garage |
| `GET` | `/players/{uid}/vgarage/{category}` | Unlocked classnames of one category |
| `POST` | `/players/{uid}/vgarage/{category}` | Unlock classnames |
| `DELETE` | `/players/{uid}/vgarage/{category}/{classname}` | Remove one unlocked classname |
| `POST` | `/players/{uid}/locker` | Create an empty locker (409 if it exists) |
| `GET` | `/players/{uid}/locker` | One player's locker |
| `PUT` | `/players/{uid}/locker` | Replace the whole locker (bulk sync, max 25 items) |
| `DELETE` | `/players/{uid}/locker` | Remove a whole locker |
| `POST` | `/players/{uid}/locker/items` | Store an item (`category`, `classname`, `amount`) |
| `PATCH` | `/players/{uid}/locker/items/{classname}` | Change an item's amount |
| `DELETE` | `/players/{uid}/locker/items/{classname}` | Remove an item |
| `POST` | `/players/{uid}/vlocker` | Create an empty virtual locker |
| `GET` | `/players/{uid}/vlocker` | Unlocked equipment classnames |
| `DELETE` | `/players/{uid}/vlocker` | Remove a whole virtual locker |
| `GET` | `/players/{uid}/vlocker/{category}` | Unlocked classnames of one category |
| `POST` | `/players/{uid}/vlocker/{category}` | Unlock classnames |
| `DELETE` | `/players/{uid}/vlocker/{category}/{classname}` | Remove one unlocked classname |
| `POST` | `/players/{uid}/phone` | Initialize a phone (201 new, 200 existing) |
| `GET` | `/players/{uid}/phone` | Whole phone payload (contacts, messages, emails) |
| `PUT` | `/players/{uid}/phone` | Replace the whole payload (bridge migration seed) |
| `DELETE` | `/players/{uid}/phone` | Remove a whole phone |
| `POST` | `/players/{uid}/phone/contacts` | Add a contact |
| `DELETE` | `/players/{uid}/phone/contacts/{contactUid}` | Remove a contact |
| `GET` | `/players/{uid}/phone/messages` | List messages |
| `POST` | `/players/{uid}/phone/messages` | Send a message (optional client-supplied `id`; idempotent per phone) |
| `GET` | `/players/{uid}/phone/messages/{otherUid}` | Message thread with one player |
| `POST` | `/players/{uid}/phone/messages/{messageId}/read` | Mark a message read |
| `DELETE` | `/players/{uid}/phone/messages/{messageId}` | Delete a message (own copy only) |
| `GET` | `/players/{uid}/phone/emails` | List emails |
| `POST` | `/players/{uid}/phone/emails` | Send an email (optional client-supplied `id`; idempotent per phone) |
| `POST` | `/players/{uid}/phone/emails/{emailId}/read` | Mark an email read |
| `DELETE` | `/players/{uid}/phone/emails/{emailId}` | Delete an email (own copy only) |
| `POST` | `/players/{uid}/actor` | Create an actor (409 if it exists) |
| `GET` | `/players/{uid}/actor` | One player's actor |
| `PUT` | `/players/{uid}/actor` | Replace the whole actor (hot-state save; creates if missing) |
| `DELETE` | `/players/{uid}/actor` | Remove an actor |
| `POST` | `/orgs` | Register an org (201; 409 if the id exists) |
| `GET` | `/orgs/{id}` | One org |
| `PUT` | `/orgs/{id}` | Replace the whole org record (hot-state save; creates if missing) |
| `DELETE` | `/orgs/{id}` | Remove an org record (members must be moved first, as disband does) |
| `GET` | `/orgs/invites?memberUid=…` | Pending invites of one player, across orgs |
| `GET` | `/orgs/{id}/members` | Member roster |
| `POST` | `/orgs/{id}/members` | Ensure a player is on the roster |
| `DELETE` | `/orgs/{id}/members/{uid}` | Remove a member |
| `POST` | `/orgs/{id}/invites` | Invite a player (leader/CEO only) |
| `POST` | `/orgs/{id}/invites/accept` | Accept an invite |
| `POST` | `/orgs/{id}/invites/decline` | Decline an invite |
| `POST` | `/orgs/{id}/leave` | Leave an org (owner must disband instead) |
| `POST` | `/orgs/{id}/disband` | Disband an org (owner only) |
| `GET` | `/orgs/{id}/assets` | Org assets |
| `POST` | `/orgs/{id}/assets` | Add assets |
| `GET` | `/orgs/{id}/fleet` | Org fleet |
| `POST` | `/orgs/{id}/fleet` | Add fleet vehicles |
| `POST` | `/orgs/{id}/credit-lines` | Assign a member's credit line (leader/CEO only) |
| `POST` | `/orgs/{id}/credit-lines/repay` | Repay a member's credit line |
| `POST` | `/orgs/{id}/charge` | Charge org funds or a credit line |
| `POST` | `/orgs/{id}/reputation` | Adjust org reputation by a delta (reward hook) |
| `POST` | `/store/checkout` | Buy a cart (charges + grants; see above) |

Without a database connection the player-data routes answer `503
database unavailable` (SurrealDB errors answer `502`); `/health` is
unaffected. Mutations go through the web UI / server bridge as JSON —
Arma's `webRequest` documents no request-body key, so there is no SQF
example for them.

## Roadmap

- [x] Skeleton: health endpoint, SurrealDB connection, `arma://` CORS, Docker
- [x] CAD: groups, group status, contract availability (+ assigned group)
- [x] Task / contract lifecycle: assign / accept / decline, completion
- [x] Player data: bank, garages, lockers, phones, orgs, stores
- [x] Thin the Arma extension down to a bridge: player data (actor, bank,
      garages, lockers, phones, orgs) persists only through the API
- [ ] Auth for game servers and web UI clients
- [ ] Contract completion log (task/CAD state itself stays per-mission in the
      extension)
- [x] Remove the extension's legacy SurrealDB migration path

## License

TBD — expected to match the Forge project license; to be confirmed.
