---
title: Group API
tags:
  - sismatic
  - design-note
  - cqrs
  - http-api
  - rust
  - breaking-change
branch: group-api
date: 2026-08-20
---

# Group API

A `/v1/groups` space over the read and write sides both, a new store port
holding what each device group was last told to be, and a comparison that turns
the two into a drift verdict.

```text
GET  /v1/groups/{id}/fields                    every field, every member
GET  /v1/groups/{id}/fields/{field}            one field, every member
GET  /v1/groups/{id}/fields/{field}/history    one field over time, per member

POST /v1/groups/{id}/recording/start           the five write verbs, addressed
POST /v1/groups/{id}/recording/stop            to a device group
POST /v1/groups/{id}/recording/pause
PUT  /v1/groups/{id}/metadata/{field}
PUT  /v1/groups/{id}/settings/{field}

GET  /v1/groups/{id}/recording                 every member's phase
GET  /v1/groups/{id}/commands                  what each member was asked
```

> [!danger] Breaking
> `/v1/devices/{id}` no longer accepts a device group id. Eleven operations
> change behavior; see [[#14. Migration]].

## Contents

- [[#1. Starting state]]
- [[#2. The route space]]
- [[#3. Group state in the store]]
- [[#4. The comparison]]
- [[#5. The wire contract]]
- [[#6. Assembly in the handler]]
- [[#7. Changes by crate]]
- [[#8. The Ports product type]]
- [[#9. Helpers lifted out of the device routes]]
- [[#10. Test layering]]
- [[#11. Algebraic summary]]
- [[#12. Known limits and suboptimal points]]
- [[#13. Verification]]
- [[#14. Migration]]

---

## 1. Starting state

### 1.1 The shape of the system

Sismatic is split along a CQRS seam. `sismatic-sync` polls devices through
`sismatic-core` and writes `Reading` rows through a `WriteStore`.
`sismatic-http-api` answers questions about those rows through a `ReadStore`.
`sismatic-intent-relay` drains recorded intents and applies them to devices
through a `CommandDrain`. The three meet at ports and nowhere else, and only
`sismatic-server` — the composition root — knows that the object behind
`ReadStore` and `WriteStore` is one value, and that the object behind
`CommandSubmit`, `CommandLog`, and `CommandDrain` is another.

The load-bearing rule of the crate graph is that no front end has a compile
path to `sismatic-core`. `sismatic-api-types` sits at the bottom of every client
dependency chain, so a single edge from there to `core` would hand every client
a compile path to `russh` and the device model. This is why `FieldName` is a
`String` alias rather than a mirrored enum, and why `sismatic-api-types` re-declares
the value model (`ReadingValue`) instead of re-exporting `core::protocol::Value`.

### 1.2 What a group already was

Before this change, a group existed on the write side only:

| Surface | Group support |
| --- | --- |
| `sismatic-core::devices::group::DeviceGroup` | Fan-out of one `Instruction` to every member, concurrently |
| `DeviceCatalog::group` / `::members` | Configured membership as DTOs |
| `Submission { targets, batch, barrier }` | One row per member, admitted atomically, optionally under a rendezvous |
| `GET /v1/groups`, `GET /v1/groups/{id}` | Inventory: id, members, barrier policy |

Reads were device-scoped. `GET /v1/devices/{id}/fields/RUNNING_STATE` answered
for one device. A five-member device group required five requests and a
client-side join.

### 1.3 The gap that motivated a stored group state

The outbox tracks a `Phase` per device, moved by the `admit` table when a
submission is accepted and rolled back by `rollback` when a command fails
terminally. Consider a five-member device group told to start, under
`Barrier::FailBatch`, where the barrier never fills:

1. `submit` admits all five. Each `DeviceLog.phase` moves `Idle -> Recording`.
2. The barrier times out. `FailBatch` fails every row without contacting a device.
3. `rollback(Recording, Start)` returns each phase to `Idle`.
4. The next poll of `RUNNING_STATE` reports `stopped` on all five.

Read member by member the fleet is perfectly consistent: five idle recorders
whose phases agree with their readings and with each other. Nothing anywhere
records that a lecture was supposed to be running. Member-versus-member
comparison cannot detect this, because the members do agree.

---

## 2. The route space

### 2.1 Symmetry with the device routes

The three group routes mirror the three device readings routes exactly:

| Device route | Group route | Response |
| --- | --- | --- |
| `/devices/{id}/fields` | `/groups/{id}/fields` | `GroupFieldStateList` |
| `/devices/{id}/fields/{field}` | `/groups/{id}/fields/{field}` | `GroupFieldState` |
| `/devices/{id}/fields/{field}/history` | `/groups/{id}/fields/{field}/history` | `GroupHistory` |

Field-name normalization is shared (`normalize_field`), so `running-state`,
`running_state`, and `RUNNING_STATE` name one field on both spaces. The history
filters are the same `ReadingQuery` (`field`, `start`, `end`, `limit`), rendered
into the OpenAPI document from the same `IntoParams` derive. The store call
under each group handler is the device handler's call, run once per member.

`{field}` remains a path parameter passed through to the store rather than a
symbol the crate was compiled against. A field added to core's query catalog is
expanded by the `'*'` sync schedule, polled, stored, and then served on both
spaces with no code change in any crate.

### 2.2 One namespace, two spaces

Devices and groups share one id namespace — the config layer guarantees they
never collide — and the API now has two URL spaces over it. Each accepts only
its own kind, enforced by two rules in `routes::target`:

| Helper | Used by | Rule |
| --- | --- | --- |
| `group_members(catalog, id, device_route)` | every `/v1/groups` route | the id must name a device group; a device id is a `404` naming the `/v1/devices` URL |
| `reject_group(catalog, id, group_route)` | every `/v1/devices` route | the id must not name a device group; if it does, a `404` naming the `/v1/groups` URL |

`reject_group` is deliberately **not** an existence check. An id that names
nothing passes it, and each route keeps its own answer — which is what preserves
the readings routes' empty list for an unknown device. Only a *positive* group
hit is a claim, and it is one the catalog is entitled to make: it holds the
configured set, so "this id is a device group" is a fact rather than an
inference from absence.

The five `/v1/groups` write verbs are not a second code path. All ten write
routes funnel into one private `submit` over an already-resolved target list:

```rust
async fn submit_device(catalog, port, stamp, device, group_route, intent, key) {
    reject_group(catalog, &device, group_route).await?;
    if catalog.device(&device).await.is_none() { /* 404 */ }
    submit(port, stamp, vec![device], None, intent, key).await
}

async fn submit_group(catalog, port, stamp, group, device_route, intent, key) {
    let targets = group_members(catalog, &group, device_route).await?;
    let summary = catalog.group(&group).await;
    submit(port, stamp, targets, summary, intent, key).await
}
```

Expansion happens in `submit_group` rather than at dispatch because admission is
per device: a device group where one member is already recording and one is idle
has to be decided against both phases, and only a submission naming both can be.

The `202` shape is identical across both spaces, and always was:
`Acceptance { batch, commands }` is a list even for a single device, precisely
so the response type does not depend on which kind of id was used. That is why
the write side needed no new response DTO and the read side needed six.

> [!danger] Breaking change
> `/v1/devices/{id}` previously accepted a group id and fanned the write out
> across the members. It now answers `404`. Every group-addressed call must move
> to `/v1/groups/{id}`, and the refusal body carries the exact URL.
>
> Affected: the five write verbs, both status reads, all three readings routes,
> and `GET /v1/devices/{id}` — eleven operations, pinned as one property by
> `every_device_route_refuses_a_device_group_id_and_names_the_group_url`.

### 2.3 The two status routes that could not be aliases

`GET /v1/devices/{id}/recording` and `GET /v1/devices/{id}/commands` are why the
split is a refusal rather than a fan-out. The outbox keys its logs by **device**:

```rust
async fn phase(&self, device: DeviceId) -> Result<RecordingPhase, ReadError> {
    let (phase, epoch) = state.logs.get(&device)
        .map_or((Phase::Idle, 0), |log| (log.phase, log.epoch));
    Ok(RecordingPhase { phase, epoch })
}
```

A group id has no log, so it took the `map_or` default. The route answered
`{"phase":"idle","epoch":0}` for a device group whose members were
mid-recording, and `commands_for` answered `[]` for one whose members each held
a queue. Both were confident answers about a device that does not exist, and no
wording of the documentation made them safe.

Fanning out would not have fixed them either: there is no single `Phase` to
report for a set of members, and no total order over commands two submissions
stamped at the same instant. The answers had to change shape, which is what
`GroupPhase` and `GroupCommandList` are — so the device routes refuse, and the
group routes answer.

The other nine operations *could* have stayed permissive; they are refused for
consistency. A caller learning that `/v1/devices/{group}/recording/start` is
refused while `/v1/devices/{group}/fields` returns `[]` would be learning a rule
with an exception in it.

### 2.4 The tag axis

The three operations carry `tag = "readings"`, not a tag of their own. The
document's tag vocabulary is `readings`, `inventory`, `commands`, `health`, and
it classifies by the **question a route answers** rather than by the resource its
path names:

| Tag | Devices | Groups |
| --- | --- | --- |
| `readings` | `/devices/{id}/fields...` | `/groups/{id}/fields...` |
| `inventory` | `/devices`, `/devices/{id}` | `/groups`, `/groups/{id}` |
| `commands` | `/devices/{id}/...` | same routes, group id |

A `groups` tag was written first and then removed. It classified on the other
axis, and the two consequences were concrete:

- `/v1/groups/{id}` and `/v1/groups/{id}/fields` would sit in different sections
  of one document, while `/v1/groups` sat beside `/v1/devices` in a third.
- Tags become classes in most client generators. A `GroupsApi` that does not
  contain `listGroups` or `getGroup` — because those are `InventoryApi`
  operations — is a worse client surface than a `ReadingsApi` with six methods.

The cost paid is one tag description that now carries two `404` stories: the
`/devices` half answers from the store alone and cannot distinguish an unknown
device from a silent one, while the `/groups` half consults the catalog and
therefore can. `inventory`'s description already sets that precedent by
explaining why its own routes diverge from `readings`.

`tests/openapi.rs::tags_name_the_question_a_route_answers_not_the_resource_it_names`
pins the vocabulary, asserts every operation carries exactly one declared tag,
and asserts the two pairings that encode the axis.

### 2.5 An unknown group is a `404`; an unreported field is not

The device readings routes answer an unknown device with an empty list. The
store holds what the sync side wrote and no catalog of what could be written, so
it cannot distinguish "no such device" from "this device has not answered yet",
and a `404` would report an unreachable device as an unconfigured one.

The group routes have no such constraint. They cannot answer at all without
asking the catalog which devices the id addresses, and the catalog is the
configured set. By the time a group could be answered for, its existence has
already been settled, so `404` is a true claim.

A field no member has reported is a `200`:

```json
{
  "group": "atrium-room",
  "field": "TIMEZONE",
  "expected": null,
  "sync": "unknown",
  "uniform": true,
  "members": [
    { "device": "atrium", "reading": null, "sync": "unknown" },
    { "device": "annex",  "reading": null, "sync": "unknown" }
  ]
}
```

The group exists, the members are known, and naming the silent members is the
answer. On the device route the equivalent response would be an empty shell
carrying no information, which is why that route answers `404` instead.

## 3. Group state in the store

### 3.1 What is recorded

`sismatic-store::group` adds a read port over a mapping:

```text
groups: GroupId -> FieldName -> GroupExpectation { field, value, since }
```

`GroupExpectation` is what the device group was last **told** one of its fields
should
hold. `since` is the submission's instant, shared by every member of one
request.

```rust
#[async_trait::async_trait]
pub trait GroupState: Send + Sync {
    async fn expected(&self, group: GroupId, field: FieldName)
        -> Result<Option<GroupExpectation>, ReadError>;
    async fn expected_all(&self, group: GroupId)
        -> Result<Vec<GroupExpectation>, ReadError>;
}
```

Absence is never an error, matching `ReadStore`. A group nobody has commanded
has no expectation, and that is an answer rather than a failure.

`expected_all` exists as a method rather than a loop over `expected` for the
reason `ReadStore::latest_all` does: the index route needs the whole set, and an
adapter answers it in one pass, whereas a caller looping would first need a
field catalog it has no way to obtain.

### 3.2 Where the mapping lives

The mapping lives on `MemoryOutbox`, not on `MemoryStore`.

An expectation is write-side belief. It is the same kind of thing a `Phase` is:
a claim about what the system intends, produced by accepting a request, and not
an observation of a device. `MemoryStore` holds observations. Placing the two in
one adapter would make "what the device group was told" and "what a device
reported"
two rows in one table, which is exactly the conflation the group routes exist
to undo.

The practical consequence is atomicity. Recording the expectation inside
`MemoryOutbox::submit` puts it under the same `Mutex` that guards `logs` and
`batches`, in the same critical section as the admission decision. The
alternative — a separate `GroupStateWrite` port called by the HTTP layer after
`submit` returns — was rejected on three counts:

1. It would grant the HTTP surface a second write verb. `startup::run`'s
   argument list narrows capability deliberately, and the write side has exactly
   one verb that records a request rather than performing one.
2. It would be non-atomic. A process that died between the two calls would leave
   a queued command with no expectation, or an expectation with no command.
3. It would permit an expectation with no submission behind it at all, which is
   a claim that a device group was asked for something no device will ever be
   told.

`Submission` gained one field to carry the group id across the seam:

```rust
pub struct Submission {
    pub ids: Vec<CommandId>,
    pub targets: Vec<DeviceId>,
    /// The group this request was addressed to, when it was addressed to one.
    pub group: Option<GroupId>,
    pub batch: Option<BatchId>,
    pub barrier: Option<BarrierPolicy>,
    pub intent: Intent,
    pub at: Timestamp,
    pub idempotency_key: Option<String>,
}
```

`group` is carried alongside the expanded `targets` rather than in place of
them. The two are needed for different things: the members are what admission is
decided against, and the group is what the expectation is filed under.

### 3.3 When it is written, and when it is not

The insert sits after the last fallible step in `submit` and before the batch is
armed:

```rust
if let Some(group) = &s.group {
    let (field, value) = expects(&s.intent);
    state.groups.entry(group.clone()).or_default().insert(
        field.clone(),
        GroupExpectation { field, value, since: s.at.clone() },
    );
}
```

Three paths return before it, each deliberately:

| Path | Effect on the expectation |
| --- | --- |
| Malformed submission (no targets, id/target length mismatch) | Untouched |
| Refused by `admit` (for example a second `Start` while `Recording`) | Untouched — the previous expectation and its `since` stand |
| Idempotent replay under an existing key | Untouched — no new commands were produced, so `since` must not move forward |

A device-addressed request carries `group: None` and files nothing. Starting one
device does not speak for the device group it happens to belong to; filing an
expectation there would report every other member as drifted for doing nothing
wrong.

### 3.4 No rollback on failure

`rollback` exists because a failed start must stop freezing metadata: without
it, one unreachable recorder makes its own metadata permanently unwritable. The
phase gates admission, so a stuck phase is an outage.

The expectation gates nothing. It is read by a dashboard and by nothing else.
Rolling it back on failure would erase precisely the record that makes the
[[#1.3 The gap that motivated a stored group state|abandoned-take scenario]]
detectable, converting the one situation worth an alarm into a device group that
is quietly idle.

Traced against the same five-recorder failure:

| Step | Phase | Expectation | Latest reading |
| --- | --- | --- | --- |
| Before | `Idle` | none | `stopped` |
| After `submit` | `Recording` | `RUNNING_STATE = started`, `since = T0` | `stopped` |
| After barrier timeout and `FailBatch` | `Idle` | `RUNNING_STATE = started`, `since = T0` | `stopped` |
| Group route reports | — | `sync: drifted`, `uniform: true` | — |

The device group is uniform and drifted: the members agree with each other, and
none of
them agrees with what was asked.

### 3.5 Keyed by field, not by phase

An expectation shaped as a `Phase` would cover the three lifecycle verbs and
nothing else. Keyed by field it also covers `SetMetadata` and `SetSetting`, so a
device group where four members took a new title and one did not is the same
kind of finding as a device group where four started, reported through one shape
on the same
routes.

The derivation is total and wildcard-free:

```rust
pub fn expects(intent: &Intent) -> (FieldName, ReadingValue) {
    let state = |state| (RECORDING_STATE_FIELD.to_owned(), ReadingValue::State(state));
    match intent {
        Intent::StartRecording => state(RecordingState::Started),
        Intent::StopRecording => state(RecordingState::Stopped),
        Intent::PauseRecording => state(RecordingState::Paused),
        Intent::SetMetadata { field, value } | Intent::SetSetting { field, value } => {
            (field.clone(), ReadingValue::Text(value.clone()))
        }
    }
}
```

A new `Intent` variant stops this compiling until someone says what it expects.
A default arm would silently ship a lifecycle verb that stopped keeping a device
group in step, so no default arm exists. The pattern matches `Verb::of`, `admit`,
`reconcile`, and `rollback`, all of which are total functions written out as
tables in the same crate.

### 3.6 The one string that crosses the catalog seam

`SetMetadata` and `SetSetting` name their own field. The three lifecycle verbs
do not, so the store has to name the field the recording lifecycle is observed
on, and the store cannot see core's query catalog.

```rust
pub const RECORDING_STATE_FIELD: &str = "RUNNING_STATE";
```

Options considered:

| Approach | Assessment |
| --- | --- |
| Constant in `sismatic-store`, held down by a test from a crate that sees both | Adopted |
| Constant in `sismatic-api-types` | Rejected: that crate deliberately mirrors no part of core's catalog, and one entry is the start of the mirror |
| Plumbed from the composition root as configuration | Rejected: a runtime parameter for a compile-time fact, threaded through two crates for one string |
| Match on `ReadingValue::State(_)` rather than a field name | Rejected: the readings API is keyed by field name, so a route still has to name the field it queries |

`sismatic-sync` can see both crates and already resolves the same name from the
catalog for its reconciliation hook. The sentinel lives beside that function:

```rust
fn is_running_state(field: &str) -> bool {
    field == Query::RunningState.name()
}

#[test]
fn the_stores_recording_field_is_the_name_this_driver_polls_it_under() {
    assert_eq!(
        sismatic_store::group::RECORDING_STATE_FIELD,
        Query::RunningState.name()
    );
    assert!(is_running_state(sismatic_store::group::RECORDING_STATE_FIELD));
}
```

> [!danger] What a rename would cost without the sentinel
> Expectations filed under the old name, readings written under the new one, and
> every group reporting `sync: unknown` forever with nothing failing to say why.
> The failure is silent and permanent, which is the class of failure this
> codebase already guards with wildcard-free matches at every other seam
> (`sismatic_sync::dto::to_dto`, the `Barrier` mapping in
> `sismatic_server::summarize_group`).

---

## 4. The comparison

### 4.1 The asymmetry

An expectation minted from a lifecycle verb is already typed
(`ReadingValue::State`) and matches a reading exactly. An expectation minted
from a write carries the caller's text, because that is what the `Intent` held.
The device answers a decoded value.

| Written | Expectation | Device reports | `==` |
| --- | --- | --- | --- |
| `PUT /settings/HTTP_PORT {"value": "8080"}` | `Text("8080")` | `Port(8080)` | false |
| `PUT /settings/DHCP_MODE {"value": "true"}` | `Text("true")` | `Flag(true)` | false |
| `PUT /metadata/TITLE {"value": "Week 4"}` | `Text("Week 4")` | `Text("Week 4")` | true |
| `POST /recording/start` | `State(Started)` | `State(Started)` | true |

Under strict equality, every flag and port setting in the system would read as
permanently drifted. Of the eight writable settings in core's catalog, two are
`Shape::Flag` and two are `Shape::Port`, so half the setting surface would be
wrong.

### 4.2 The rule adopted

A `Text` expectation is parsed in the shape the device answered in. Everything
else is compared as itself.

```rust
pub fn satisfies(expected: &ReadingValue, observed: &ReadingValue) -> bool {
    match expected {
        _ if expected == observed => true,
        ReadingValue::Text(want) => matches_text(want.trim(), observed),
        _ => false,
    }
}

fn matches_text(want: &str, observed: &ReadingValue) -> bool {
    match observed {
        ReadingValue::Text(got) | ReadingValue::Version(got) | ReadingValue::Ack(got) => want == got,
        ReadingValue::Port(got) => want.parse::<u16>().is_ok_and(|p| p == *got),
        ReadingValue::Number(got) => want.parse::<u32>().is_ok_and(|n| n == *got),
        ReadingValue::Flag(got) => flag_of(want) == Some(*got),
        ReadingValue::Mac(got) => want.eq_ignore_ascii_case(&got.0),
        ReadingValue::State(got) => want.eq_ignore_ascii_case(state_name(*got)),
        ReadingValue::Alarms(_) => false,
    }
}
```

The direction of the parse is the design point. The device's decode is the
authority on what kind of value a field holds. `sismatic-store` does not know
that `HTTP_PORT` is a port and does not have to; the reading says so. The
alternative direction — rendering the reading to text and comparing strings —
requires the store to commit to a canonical text form for every variant, which
is a second encoding to keep in step with core's wire form.

`matches_text` is wildcard-free, so a new `ReadingValue` variant stops it
compiling. The `Text | Version | Ack` arm is reached only when the two strings
differ, since `satisfies` tried equality first; it is written out rather than
wildcarded to keep the match total.

### 4.3 The mirrored spellings, and the direction of the error

```rust
fn flag_of(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}
```

These mirror `sismatic-core`'s `Setting::encode`, which is private, so no
compile-time or test-time link binds the two. A spelling core accepts and this
does not reads as `drifted` on a device that is in fact correct.

The error is a false alarm rather than a missed one. For a signal whose entire
purpose is to be noticed, a false positive is visible and correctable while a
false negative is indistinguishable from health. This is stated in the
function's own documentation rather than left for a reader to discover.

> [!warning] Suboptimal
> This is the weakest link in the change. Making core's flag vocabulary public
> — for example `Setting::flag_spellings() -> &'static [(&str, bool)]` — would
> allow a sentinel in `sismatic-sync` of the same form as the one guarding
> `RECORDING_STATE_FIELD`. That was not done, so the mirror is held only by
> prose.

### 4.4 What satisfies is not

`satisfies` is a binary relation, not an equality:

- Not symmetric. `satisfies(Text("8080"), Port(8080))` is true;
  `satisfies(Port(8080), Text("8080"))` is false, because the second argument's
  shape is what the first is read into.
- Not transitive. `Text("1")` satisfies `Flag(true)` and satisfies `Number(1)`,
  but `Flag(true)` and `Number(1)` do not satisfy each other.
- Reflexive on the diagonal, by the `expected == observed` arm.

The argument order encodes the roles: `satisfies(expected, observed)`. Reversing
them at a call site is a silent wrong answer, which the parameter names and the
single call site in `assemble` are the only defense against.

---

## 5. The wire contract

All new DTOs live in `sismatic-api-types::group`, derive `Serialize`,
`Deserialize`, and — behind the existing feature flags — `ts_rs::TS` and
`utoipa::ToSchema`.

### 5.1 The types

```rust
pub struct GroupExpectation {
    pub field: FieldName,
    pub value: ReadingValue,
    pub since: Timestamp,
}

pub enum SyncState { InSync, Drifted, Unknown }   // snake_case on the wire

pub struct MemberState {
    pub device: DeviceId,
    pub reading: Option<Reading>,
    pub sync: SyncState,
}

pub struct GroupFieldState {
    pub group: GroupId,
    pub field: FieldName,
    pub expected: Option<GroupExpectation>,
    pub sync: SyncState,
    pub uniform: bool,
    pub members: Vec<MemberState>,
}

pub struct GroupFieldStateList { pub group: GroupId, pub fields: Vec<GroupFieldState> }
pub struct MemberHistory       { pub device: DeviceId, pub readings: Vec<Reading> }
pub struct MemberCommands      { pub device: DeviceId, pub commands: Vec<CommandRecord> }
pub struct GroupCommandList    { pub group: GroupId, pub members: Vec<MemberCommands> }
pub struct MemberPhase         { pub device: DeviceId, pub phase: Phase, pub epoch: u64 }

pub struct GroupPhase {
    pub group: GroupId,
    /// The phase every member is in, or `null` when they are not all in one.
    pub phase: Option<Phase>,
    pub members: Vec<MemberPhase>,
}
pub struct GroupHistory {
    pub group: GroupId,
    pub field: FieldName,
    pub expected: Option<GroupExpectation>,
    pub members: Vec<MemberHistory>,
}
```

Every `DeviceId`, `GroupId`, and `FieldName` field carries
`#[cfg_attr(feature = "openapi", schema(value_type = String))]`. Without it a
derive sees only the alias name it was written with, utoipa invents a component
named `String`, and every generated client grows a wrapper type around a plain
string. `tests/openapi.rs::the_string_aliases_are_documented_as_strings` fails if
one is dropped.

### 5.2 `SyncState` is three-valued

A boolean would have to fold "we cannot tell" into either `true` or `false`.
Both are wrong claims on a dashboard: `true` reports an uncommanded group as
healthy, `false` reports it as broken. `Unknown` is the resting state of a
system nobody has asked for anything yet, and it arises in two distinct
situations that a client does not need to distinguish:

- no expectation is recorded for the field;
- the member has never reported the field.

### 5.3 Uniform is a boolean beside a three-valued enum

The asymmetry is deliberate. Agreement with an expectation is not always
decidable, because an expectation may not exist. Agreement among members always
is: with fewer than two reporters it is vacuously true, and vacuity is a fact
about the answer rather than an absence of one.

The two comparisons fail independently, and neither subsumes the other:

| Situation | `sync` | `uniform` |
| --- | --- | --- |
| Device group started when told to | `in_sync` | `true` |
| One member did not start | `drifted` | `false` |
| No member started (abandoned take) | `drifted` | `true` |
| Firmware differs, nobody commanded it | `unknown` | `false` |
| Nothing commanded, nothing reported | `unknown` | `true` |

Rows three and four are the load-bearing cases. Row three is invisible to
member-versus-member comparison; row four is invisible to the expectation.

### 5.4 A silent member is `null`, not omitted

`MemberState.reading` is `Option<Reading>` with no `skip_serializing_if`, so it
serializes as `null`. Dropping silent members would make a five-member device
group with one silent member render identically to a four-member device group.
Which member went quiet
is the answer.

The same applies to `MemberHistory.readings`, which is an empty array rather
than an absent member, and to `expected`, which is `null` rather than absent.

### 5.5 `GroupPhase.phase` is an `Option`, not a fourth state

A device group has no phase of its own. The outbox admits per member — a start
has to be decided against each member's own state — so the only honest
group-level answer is the one the members agree on, and `null` when they do not.

That is the write-side counterpart of [[#5.3 Uniform is a boolean beside a three-valued enum|`uniform`]],
and it is spelled differently for a reason: `uniform` answers "do they agree?"
about values the response also carries, while `phase` answers "what do they
agree on?" and therefore has to carry the agreed value or nothing. Adding a
separate `uniform: bool` beside it would make `phase: null, uniform: true` a
representable state that means nothing.

`null` covers the empty device group too: there is no phase every member is in
when there is no member.

Epochs are never rolled up. Two members can share a phase and be on different
takes, and a single group epoch would have to pick one of them.

### 5.6 A worked response

Two members, `atrium` first and `annex` second in configured order. The device
group
was told to start at `T0`; `annex` did not.

```json
{
  "group": "atrium-room",
  "field": "RUNNING_STATE",
  "expected": {
    "field": "RUNNING_STATE",
    "value": { "type": "state", "value": "started" },
    "since": "2026-08-17T00:00:00.000Z"
  },
  "sync": "drifted",
  "uniform": false,
  "members": [
    {
      "device": "atrium",
      "reading": {
        "device": "atrium",
        "field": "RUNNING_STATE",
        "value": { "type": "state", "value": "started" },
        "at": "2026-08-17T00:00:05.000Z"
      },
      "sync": "in_sync"
    },
    {
      "device": "annex",
      "reading": {
        "device": "annex",
        "field": "RUNNING_STATE",
        "value": { "type": "state", "value": "stopped" },
        "at": "2026-08-17T00:00:05.000Z"
      },
      "sync": "drifted"
    }
  ]
}
```

Members appear in configured order, not sorted. `MemoryCatalog` sorts devices
and groups by id at construction but leaves a group's member list alone: the
operator wrote the sequence, and a fan-out that reordered it would address the
device group differently than it reads. The black-box tests use `[atrium,
annex]`, which
is not alphabetical, so a response that came back sorted fails visibly rather
than passing by coincidence.

---

## 6. Assembly in the handler

### 6.1 One fold, two routes

`assemble` computes both comparisons in one pass and is called by both
latest-value routes:

```rust
fn assemble(
    group: String,
    field: FieldName,
    expected: Option<GroupExpectation>,
    observed: impl Iterator<Item = (DeviceId, Option<Reading>)>,
) -> GroupFieldState
```

The two routes differ only in how they obtained the readings — `latest` per
member versus one `latest_all` per member, filtered by field — so the verdict
logic is shared. A `sync` that meant something different on the index than on
the detail view would be a contract that cannot be read.

The per-member verdict:

```rust
let sync = match (&expected, &reading) {
    (None, _) | (_, None) => SyncState::Unknown,
    (Some(expected), Some(reading)) => {
        if satisfies(&expected.value, &reading.value) {
            SyncState::InSync
        } else {
            SyncState::Drifted
        }
    }
};
```

Uniformity, vacuously true below two reporters:

```rust
let mut reported = members.iter().filter_map(|m| m.reading.as_ref());
let uniform = match reported.next() {
    None => true,
    Some(first) => reported.all(|r| r.value == first.value),
};
```

The roll-up, in which `Drifted` absorbs:

```rust
let sync = if members.iter().any(|m| m.sync == SyncState::Drifted) {
    SyncState::Drifted
} else if members.iter().any(|m| m.sync == SyncState::InSync) {
    SyncState::InSync
} else {
    SyncState::Unknown
};
```

A roll-up is read as a status light. A device group where four members started
and one did not needs attention; it is not four-fifths fine. The ordering of the
arms is the whole content of that claim.

Note that `uniform` uses `==` on `ReadingValue` while `sync` uses `satisfies`.
This is correct: two members' readings are both decoded device values, so they
are comparable as themselves, and the asymmetric rule has no role.

### 6.2 The index route and the union of field sets

`GET /groups/{id}/fields` reports the union of

- every field any member has reported, and
- every field the group has an expectation for.

The second half matters: a field the device group was told to set but no member
has answered on yet is what a write that reached nobody looks like, and the store
holds nothing for it.

```rust
let mut fields: BTreeSet<FieldName> = expectations
    .iter()
    .map(|expectation| expectation.field.clone())
    .collect();
for device in &readings {
    fields.extend(device.iter().map(|reading| reading.field.clone()));
}
```

`BTreeSet` supplies both de-duplication and the field ordering the response
promises, so nothing is sorted afterward. This mirrors the reasoning for
`MemoryStore`'s inner `BTreeMap` and for `expected_all`.

### 6.3 History, and a per-member limit

`limit` bounds each member's series, not the response:

```rust
for device in member_ids {
    let mut readings = store.between(device.clone(), field.clone(), span.clone()).await?;
    truncate(&mut readings, query.limit);
    members.push(MemberHistory { device, readings });
}
```

A caller asking for the last hundred points of `RUNNING_STATE` in a five-member
device group
wants a hundred points each. A shared budget would return whichever member the
loop reached first and truncate the rest to nothing.

The cost is stated rather than hidden: the response is bounded by
`limit x members` rather than by `limit`, with `MAX_LIMIT` at 10 000 per series.
A ten-member device group therefore has a ceiling of 100 000 rows.

`expected` is read after the series rather than before. An expectation newer
than the readings is the honest ordering for "what it should be, and what it has
been"; the reverse would let a slow store report a target that was already
superseded when the readings were taken.

### 6.4 Read amplification

Every group route performs one store call per member:

| Route | Store calls | Group-state calls |
| --- | --- | --- |
| `/fields` | `n` x `latest_all` | 1 x `expected_all` |
| `/fields/{field}` | `n` x `latest` | 1 x `expected` |
| `/fields/{field}/history` | `n` x `between` | 1 x `expected` |

Against `MemoryStore` this is `n` `DashMap` lookups and is free. Against a SQL
adapter it is a classic N+1. The `ReadStore` port has no batch method, and
adding one (`latest_all_of(&[DeviceId])`) would be a port change that every
adapter must implement for a workload that is currently tens of devices.

> [!warning] Suboptimal, deliberately deferred
> The loop is sequential rather than concurrent. `DeviceGroup::run` spawns every
> member's exchange before awaiting any, precisely so members act in unison; the
> read path does not, because a store read is not a device exchange and the
> members do not need to be read simultaneously. A SQL-backed adapter would want
> either `futures::try_join_all` here or a batched port method, and the batched
> port is the better of the two: it lets the adapter issue one query with an
> `IN` clause rather than `n` round trips in parallel.

---

## 7. Changes by crate

### 7.1 `sismatic-api-types`

| File | Change |
| --- | --- |
| `src/group.rs` | New. Eleven DTOs described in [[#5. The wire contract]] |
| `src/lib.rs` | `pub mod group`, eleven re-exports, layout paragraph in the crate docs |

No dependency change. The crate still depends on `serde` alone, plus the two
optional derive crates.

### 7.2 `sismatic-store`

| File | Change |
| --- | --- |
| `src/group.rs` | New. `GroupState`, `DynGroupState`, `RECORDING_STATE_FIELD`, `expects`, `satisfies`, `matches_text`, `flag_of`, `state_name`, seven unit tests |
| `src/lib.rs` | `pub mod group`, re-export of `DynGroupState` and `GroupState` |
| `src/outbox.rs` | `Submission::group`; `GroupId` import; `CommandSubmit::submit` contract extended to state that "records nothing at all" includes the expectation |

`state_name` duplicates the `snake_case` spelling that `SyncState`'s and
`RecordingState`'s `Serialize` impls produce. The same duplication already
exists as `error::phase_name`, and for the same reason: the comparison and the
rendered body must agree on what to call a state.

### 7.3 `sismatic-store-memory`

| File | Change |
| --- | --- |
| `src/outbox.rs` | `State.groups` map; expectation insert inside `submit`; `impl GroupState for MemoryOutbox`; module-doc shape block updated; six new tests in `batch_tests`; `group` field added to four existing `Submission` literals and a `GROUP` constant |

The insert reuses the existing `Mutex`. No new lock, and therefore no new lock
ordering to document — the module's stated `state`-before-`records` order is
unchanged.

### 7.4 `sismatic-http-api`

| File | Change |
| --- | --- |
| `src/routes/group_readings.rs` | New. Three handlers, `assemble`, eight unit tests |
| `src/routes/readings.rs` | `span_of`, `truncate`, `reject_conflicting_field` extracted to `pub(crate)`; `field_history` rewritten to use them; all three handlers take the catalog and `reject_group` |
| `src/routes/target.rs` | New. `group_members`, `reject_group`, `reject_group_bare` — the two-space rules |
| `src/routes/devices.rs` | `read_device` names `/v1/groups/{id}` in its 404 |
| `src/routes.rs` | Module declaration and three re-exports |
| `src/startup.rs` | `Ports` struct; `run` signature; `GroupState` registered as `app_data`; ten resources registered ahead of `/groups/{id}` |
| `src/openapi.rs` | Ten paths, five component schemas, widened `readings` tag description |
| `src/routes/commands.rs` | `submit` over a resolved target list; `submit_device` and `submit_group`; five group write handlers; `read_group_phase` and `list_group_commands`; `reject_group` on both status reads |
| `src/lib.rs` | Route table extended; `Ports` re-exported; paragraph on the read/write URL asymmetry |
| `tests/groups.rs` | New. Thirty-three black-box tests |
| `tests/harness/mod.rs` | `device_group(&[&str])` helper; `catalog()` expressed through it; `DynGroupState` wired; `run` call updated to `Ports` |
| `tests/openapi.rs` | Path counts 16 to 26 and 15 to 25; seeded submission carries a group; tag-axis test |

Route registration order follows the existing longest-path-first discipline:

```rust
.service(web::resource("/groups/{id}/fields/{field}/history").route(web::get().to(group_field_history)))
.service(web::resource("/groups/{id}/fields/{field}").route(web::get().to(read_group_field)))
.service(web::resource("/groups/{id}/fields").route(web::get().to(list_group_fields)))
.service(web::resource("/groups/{id}").route(web::get().to(read_group)))
.service(web::resource("/groups").route(web::get().to(list_groups)))
```

actix matches in registration order. A path parameter stops at `/`, so the order
is not load-bearing today, but it is the order that stays correct if a segment
ever becomes a tail match.

### 7.5 `sismatic-sync`

| File | Change |
| --- | --- |
| `src/driver.rs` | The `RECORDING_STATE_FIELD` sentinel test beside `is_running_state` |

### 7.6 `sismatic-server`

| File | Change |
| --- | --- |
| `src/lib.rs` | `group_state: DynGroupState` cloned from the outbox; `run` call converted to `Ports`; comment updated from "three capabilities" to "four" |

The outbox is now four trait objects from one value:

```rust
let outbox = MemoryOutbox::with_max_attempts(cfg.intent_relay.max_attempts);
let submit: DynCommandSubmit = Arc::new(outbox.clone());
let log: DynCommandLog = Arc::new(outbox.clone());
let group_state: DynGroupState = Arc::new(outbox.clone());
let drain: DynCommandDrain = Arc::new(outbox);
```

Each handle admits only its own trait's methods. The HTTP surface can append,
read, and read group state; the relay can drain; neither type admits the other's
verbs.

---

## 8. The Ports product type

Adding a sixth collaborator took `run` to eight parameters. `cargo clippy
--all-targets -- --deny warnings` — which `nix flake check` runs — treats
`clippy::too_many_arguments` (threshold seven) as an error.

Two responses:

| Option | Assessment |
| --- | --- |
| `#[allow(clippy::too_many_arguments)]` | Suppresses the lint without addressing what it detected |
| Group the collaborators into a struct | Adopted |

```rust
pub struct Ports {
    pub store: DynReadStore,
    pub catalog: DynDeviceCatalog,
    pub status: DynDeviceStatus,
    pub submit: DynCommandSubmit,
    pub log: DynCommandLog,
    pub group_state: DynGroupState,
}

pub fn run(listener: TcpListener, ports: Ports, stamp: Stamp) -> Result<Server, std::io::Error>
```

The lint was detecting something real. Four of the six fields are trait objects
built from one value, so at the call site they were six `Arc::new(x.clone())`
expressions over two distinguishable things, distinguished only by position.
Named fields turn a mis-wiring into a field name that does not exist; positions
turn it into a server that answers plausible nonsense.

Grouping also gives each capability a doc comment at its declaration, which is
where the argument-list narrowing previously described in prose now lives
structurally. This follows the same reasoning `TimeSpan` and `RecordingPhase`
already use in `sismatic-api-types`: values that are only meaningful together
are one product type.

Two call sites changed: `tests/harness/mod.rs` and `sismatic_server::run`.

---

## 9. Helpers lifted out of the device routes

Three fragments of `field_history` became `pub(crate)` in
`routes::readings`, because the group history route asks the same questions:

```rust
pub(crate) fn span_of(query: &ReadingQuery) -> TimeSpan
pub(crate) fn truncate(readings: &mut Vec<Reading>, limit: Option<u32>)
pub(crate) fn reject_conflicting_field(query: &ReadingQuery, field: &str) -> Result<(), ApiFailure>
```

Each encodes a policy that must not fork between the two routes:

- `span_of` — an omitted `start` means `BEGINNING_OF_TIME` and an omitted `end`
  means `END_OF_TIME`. These are lexicographic bounds, sound because RFC 3339 in
  UTC sorts lexicographically in chronological order.
- `truncate` — the page is taken from the **front**, keeping the most recent
  rows. Chronological order survives, so a plot of a limited response is a plot
  of its tail rather than of a reversed series.
- `reject_conflicting_field` — a `?field=` that disagrees with the path is a
  `400`, not an ignored parameter. Serving a caller a different field than the
  one it spelled out is the failure mode ruled out here.

The body of `field_history` went from about forty lines including comments to
eight. `normalize_field` was already `pub(crate)` for the same reason, shared
with the write routes so that `metadata/title` and `fields/TITLE` name one
field.

---

## 10. Test layering

Fifty-six tests were added across four layers.

### 10.1 Pure functions, no I/O

`sismatic-store::group` — seven tests over `expects` and `satisfies`. These need
no store, no clock, no runtime, and no task, in the same way `admit`,
`rollback`, `reconcile`, and `epoch_of` are tested in `sismatic-store::outbox`.

The coverage is stated against the domain rather than case by case where
possible. `a_text_expectation_is_read_in_the_shape_the_device_answered_in`
tables fifteen `(text, decoded)` pairs including every flag spelling;
`a_typed_expectation_is_satisfied_by_the_same_value_and_nothing_else` iterates
the three non-matching `RecordingState` values plus `Unknown`.

`sismatic-http-api::routes::group_readings` — eight tests over `assemble`,
constructed from literal `Option<Reading>` values. Each of the five rows in
[[#5.3 Uniform is a boolean beside a three-valued enum|the comparison table]]
has a test.

### 10.2 The adapter

`sismatic-store-memory::outbox::batch_tests` — six tests over the recording
rule:

| Test | Property |
| --- | --- |
| `an_admitted_group_request_records_what_the_room_was_told` | The expectation exists, with the submission's `since` |
| `an_abandoned_take_leaves_the_expectation_standing` | The scenario in [[#3.4 No rollback on failure]] |
| `a_refused_group_request_leaves_the_previous_expectation_untouched` | `since` does not move on a `409` |
| `a_device_addressed_request_records_no_group_expectation` | `group: None` files nothing |
| `expectations_accumulate_per_field_in_field_order` | One entry per field, `BTreeMap` order |
| `an_idempotent_replay_does_not_move_the_expectation` | `since` does not move on a replay |

### 10.3 Black box

`tests/groups.rs` — thirty-three tests against the real server on an ephemeral
port, over the real `MemoryStore` and the real `MemoryOutbox`.

Every expectation in this suite is created the way a client creates one, by
POSTing to a write route. A stub that recorded expectations on demand would let
a test assert drift detection over a state the server cannot reach, because the
rule that an expectation exists exactly when a request was admitted lives inside
the outbox's critical section.

The suite covers response shape and member ordering, the five comparison rows,
field-name normalization, the index union, per-member history and per-member
limit, span filtering, the `?field=` conflict, `404` on all three routes for an
unknown group, and the device-id message.

### 10.4 The path-drift guard

`tests/openapi.rs::every_documented_operation_is_one_the_server_serves` reads
the served document, fills each template's parameters from seeded fixtures, and
requests every path. A `404` or `405` means the `#[utoipa::path]` literal and
the `web::resource` literal disagree.

The suite asserts exact counts — `paths.len() == 26`, `checked == 26`,
`versioned.len() == 25` — so a route added to `startup` and forgotten in the
document, or the reverse, fails rather than passing vacuously. Both counters
were updated from 16 and 15.

Its `fill` helper already routed `/v1/groups/` templates to a group id, so the
new paths were substituted correctly without change. The seeded submission
gained `group: Some(harness::GROUP)` so that the index route has an expectation
to return and cannot answer `404` of its own accord.

> [!note] What the guard does not cover
> It closes one direction only. A route added to `startup` and never given a
> `#[utoipa::path]` is invisible to a test that starts from the document. The
> count assertions narrow this: such a route would leave the counts unchanged
> and pass, so the guard against it is the count being reviewed when a route is
> added.

### 10.5 The catalog helper

`harness::catalog()` was rewritten in terms of a new `device_group(&[&str])`,
which
builds one group over the named members and a `DeviceSummary` for each. The
member list is passed through unsorted so that configured order stays testable.

---

## 11. Algebraic summary

### 11.1 The two mappings

Both sides of the comparison are partial functions:

```text
observed  : DeviceId x FieldName ⇀ (ReadingValue, Timestamp)      -- ReadStore
expected  : GroupId  x FieldName ⇀ (ReadingValue, Timestamp)      -- GroupState
members   : GroupId → [DeviceId]                                   -- DeviceCatalog
```

Partiality is the content of `Option`, and the reason both ports state that
absence is never an error. `latest` answers `None` for three distinguishable
real-world situations — unknown device, unpolled field, never answered — that
the store cannot tell apart, and `expected` answers `None` for a group that has
never been commanded.

`assemble` is the join over `members`:

```text
GroupId x FieldName
  → (members ; observed*)  x  expected
  → GroupFieldState
```

### 11.2 `SyncState` as a bounded join-semilattice

The roll-up is a fold over a commutative, associative, idempotent operation with
`Unknown` as identity and `Drifted` as absorbing:

```text
Unknown  ∨ x        = x
InSync   ∨ InSync   = InSync
Drifted  ∨ x        = Drifted
```

`(SyncState, ∨, Unknown)` is a bounded join-semilattice, and therefore a
commutative idempotent monoid. Two consequences follow directly:

- The roll-up does not depend on member order, which is what allows the response
  to list members in configured order while the verdict stays stable.
- An empty group folds to the identity, `Unknown`, which is
  `an_empty_group_is_uniform_and_unknown`.

The `if / else if / else` chain in `assemble` is this fold written out. The
absorbing element is the first arm.

### 11.3 `uniform` as a different fold

Uniformity is not a semilattice fold over per-member verdicts, because it is not
a per-member property at all. It is the statement that the image of the reported
members under `value` is a subsingleton:

```text
uniform  ⟺  |{ m.reading.value : m ∈ members, m.reading ≠ None }| ≤ 1
```

Vacuous truth at cardinality zero and one is a property of the definition rather
than a special case, which is why `uniform` is a total `bool` while `sync` is
not.

### 11.4 `satisfies` as a relation

`satisfies ⊆ ReadingValue x ReadingValue` is reflexive and neither symmetric nor
transitive; see [[#4.4 What satisfies is not]]. It is the coproduct of two
relations: syntactic equality on the diagonal, and a partial parse
`Text x ReadingValue → Option<ReadingValue>` composed with equality. Written as
a Kleisli arrow, the second half is

```text
want ↦ parse_as(shape_of(observed))(want) >>= (== observed)
```

where `shape_of` is supplied by the observed value's own constructor. The
device's decode chooses the interpretation, which is why the relation is
directional.

### 11.5 `expects` as a total map out of a coproduct

`Intent` is a coproduct of five variants. `expects : Intent → FieldName x
ReadingValue` is total and wildcard-free, so it is defined by case on the
coproduct and the compiler enforces exhaustiveness. Adding a sixth summand
breaks the build until the new case is given a value.

This is the same discipline as `Verb::of`, `admit`, `reconcile`, `rollback`,
`sismatic_sync::dto::to_dto`, and the `Barrier` mapping in
`sismatic_server::summarize_group`. Every mapping in the workspace that crosses
a seam is written as a total function over a closed sum, and every one of them
omits the catch-all arm for the same reason: the wildcard is the only thing that
could hide a new variant.

---

## 12. Known limits and suboptimal points

| Limit | Consequence | Remedy not taken |
| --- | --- | --- |
| Flag spellings mirror a private `Setting::encode` | A spelling core accepts and the store does not reads as `drifted` on a correct device | Make the vocabulary public and add a sentinel in `sismatic-sync` |
| N+1 sequential store reads per group route | Fine for tens of devices and a `DashMap`; a SQL adapter would issue `n` round trips in series | A batched `ReadStore` method taking `&[DeviceId]` |
| `State.groups` grows without bound | One entry per `(group, field)` ever commanded, for the life of the process | Bounded retention in a durable adapter; the port does not require unbounded growth |
| Expectations are lost on restart | Same property `records` and `queue` already have | A durable outbox |
| The expectation carries no `epoch` or command id | A caller cannot ask "which take was this expectation for", nor link a drift to the command that produced it | Add `epoch` and the batch id to `GroupExpectation`; both are available inside `submit` |
| `GroupHistory.expected` describes now, not the window | A series plotted against it is plotted against a target that may postdate the window; `since` is the only signal | An expectation history keyed by time, which is a second series to store and bound |
| `limit` multiplies by member count | A ten-member device group at `MAX_LIMIT` can return 100 000 rows | A response-wide ceiling applied after the per-member truncation |
| A group-addressed call to `/v1/devices/{id}` is now a `404` | Every existing caller addressing a device group that way must change URL | Nothing; the refusal names the replacement, and the alternative was two of those routes answering wrongly forever |
| `tests/openapi.rs` asserts hardcoded path counts | Every route addition edits three integers | The counts are the only defense against a documented-but-unrouted path passing vacuously |
| The index route scans each member's field list per field | `O(fields x members)` `find` calls | Index the readings into a map before the fold; the current cost is bounded by a device's field count |

Two design choices are worth restating as deliberate rather than provisional:

- **The expectation is not rolled back on failure.** This is the property that
  makes the abandoned-take scenario detectable at all. It also means a stale
  expectation persists after an operator gives up on a device group, until the
  next group-addressed command supersedes it. That is the intended trade.
- **`Drifted` absorbs in the roll-up.** A device group that is four-fifths
  correct reports as drifted. A client wanting the proportion reads the member
  list.

---

## 13. Verification

```console
$ cargo test --workspace
384 tests passing across 30 binaries

$ cargo clippy --workspace --all-targets -- --deny warnings
clean

$ nix flake check
all checks passed!
```

The seven derivations built were `cargo-package-fmt`, `sismatic-workspace-clippy`
(`--all-targets -- --deny warnings`), `sismatic-workspace-doc`,
`sismatic-workspace-nextest`, `cargo-package-deny`, `sismatic-cli`, and
`sismatic-server`. The evaluated-only checks are `crate-audit`,
`release-plz-config-ok`, and `internal-dep-versions-ok`.

`sismatic-workspace-doc` is scoped to `cargo doc -p sismatic-core --features
testing`, so it does not cover the crates changed here. Their intra-doc links
were checked separately:

```console
$ RUSTDOCFLAGS="--deny warnings" cargo doc --no-deps --all-features \
    -p sismatic-api-types -p sismatic-store -p sismatic-store-memory -p sismatic-http-api
clean
```

> [!note] Sandbox requirement
> The flake sandbox reads the git index, not the working tree. New files must be
> staged with `git add` before `nix flake check` can see them, or the run fails
> on missing modules.

New test counts by location:

| Location | Tests |
| --- | --- |
| `sismatic-store::group` | 7 |
| `sismatic-store-memory::outbox::batch_tests` | 6 |
| `sismatic-http-api::routes::group_readings` | 8 |
| `sismatic-http-api` `tests/groups.rs` | 33 |
| `sismatic-http-api` `tests/openapi.rs` tag axis | 1 |
| `sismatic-sync::driver` sentinel | 1 |
| **Total** | **56** |

---

## 14. Migration

### 14.1 What changed

`/v1/devices/{id}` accepted a device group id before this change. A
group-addressed write was fanned out across the members; a group-addressed read
was answered from a store that holds nothing under a group id. Both now answer
`404`.

Eleven operations are affected. Each refusal carries the replacement URL:

```json
{
  "code": "not_found",
  "error": "'atrium-room' is a device group, not a device; try /v1/groups/atrium-room/recording/start"
}
```

| Was | Is now | Previous behavior for a group id |
| --- | --- | --- |
| `GET /v1/devices/{id}` | `GET /v1/groups/{id}` | `404` already, without the replacement URL |
| `GET /v1/devices/{id}/fields` | `GET /v1/groups/{id}/fields` | `200` with an empty list |
| `GET /v1/devices/{id}/fields/{field}` | `GET /v1/groups/{id}/fields/{field}` | `404`, meaning "nothing stored" |
| `GET /v1/devices/{id}/fields/{field}/history` | `GET /v1/groups/{id}/fields/{field}/history` | `200` with an empty list |
| `GET /v1/devices/{id}/recording` | `GET /v1/groups/{id}/recording` | `200` reporting `idle` at epoch `0` |
| `GET /v1/devices/{id}/commands` | `GET /v1/groups/{id}/commands` | `200` with an empty list |
| `POST /v1/devices/{id}/recording/start` | `POST /v1/groups/{id}/recording/start` | `202`, fanned out across the members |
| `POST /v1/devices/{id}/recording/stop` | `POST /v1/groups/{id}/recording/stop` | `202`, fanned out |
| `POST /v1/devices/{id}/recording/pause` | `POST /v1/groups/{id}/recording/pause` | `202`, fanned out |
| `PUT /v1/devices/{id}/metadata/{field}` | `PUT /v1/groups/{id}/metadata/{field}` | `202`, fanned out |
| `PUT /v1/devices/{id}/settings/{field}` | `PUT /v1/groups/{id}/settings/{field}` | `202`, fanned out |

The five write verbs are a rename: the request body, the `202` body
(`Acceptance { batch, commands }`), the batching rule, and the admission
semantics are all unchanged. Only the path moves.

The six reads are not a rename. Their response types are new — `GroupFieldState`,
`GroupFieldStateList`, `GroupHistory`, `GroupPhase`, `GroupCommandList` — because
the previous answers were about a device rather than about a device group. A
client following the table above has to read the new shapes; see
[[#5. The wire contract]].

### 14.2 What did not change

- **Device ids under `/v1/devices` behave exactly as before.** Nothing in the
  device space changed for a device id, including the empty-list answer for an
  unknown one.
- **An id that names nothing is unaffected.** The refusal is a claim about
  groups, not an existence check, so `GET /v1/devices/nobody/fields` is still
  `200` with an empty list and `GET /v1/devices/nobody/recording` is still
  `idle` at epoch `0`. `an_unknown_id_is_unaffected_by_the_group_refusal` pins
  it.
- **`GET /v1/commands/{id}` is unaffected.** A command id is globally unique and
  names neither kind.
- **`GET /v1/groups` and `GET /v1/groups/{id}`** — the inventory routes that
  existed before — are unchanged.
- **No storage format changed.** The outbox, the readings store and the catalog
  hold what they held; only the routing over them moved.

### 14.3 Finding affected callers

The device space no longer answers for a group id at all, so a missed caller
fails loudly rather than reading a plausible wrong answer. The failure is a
`404` whose body names the URL to use, which is enough to fix the call from the
log line alone.

Group ids are whatever the devices file declares under `[[group]]`. Grepping a
client for `/v1/devices/` and comparing the id against `GET /v1/groups` finds
every affected call site without running anything.

### 14.4 Why not a deprecation window

Nine of the eleven operations could have kept the old behavior behind a
deprecation header. Two could not:
[[#2.3 The two status routes that could not be aliases|`GET /v1/devices/{id}/recording` and `/commands`]]
report an idle device that does not exist, and there is no correct answer to
serve from them for a group — a `Phase` is per device, and a merged command list
has no total order.

Leaving those two wrong for a window would mean shipping a dashboard reading
`idle` for a device group that is recording. Leaving the other nine permissive
would mean a rule with an exception in it, which is harder to learn than the
rule. Both costs are paid once, at a `0.2.x` version, against an API with no
published client.
