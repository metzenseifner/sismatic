---
title: Fleet Read API
tags:
  - sismatic
  - design-note
  - cqrs
  - http-api
  - pagination
  - rust
branch: all-devices-state-endpoint
date: 2026-08-26
---

# Fleet Read API

One route over the whole configured device set, with two orthogonal filters and
a cursor.

```text
GET /v1/readings/devices?fields=&devices=&group=&where=&limit=&after=
```

It is the first read route in the system whose subject is not an addressable
thing. `/v1/readings/devices/{id}/fields` answers for a device;
`/v1/readings/groups/{id}/fields` answers for a configured device group; this
answers for whatever survives its filters, which is a set the caller describes
rather than one the devices file names.

Additive only. No existing route, DTO, or port changes.

## Contents

- [[#1. Starting state]]
- [[#2. Where the fleet comes from]]
- [[#3. The two filter axes]]
- [[#4. Pagination]]
- [[#5. The wire contract]]
- [[#6. Assembly in the handler]]
- [[#7. Changes by crate]]
- [[#8. Algebraic summary]]
- [[#9. Known limits and suboptimal points]]
- [[#10. Verification]]

---

## 1. Starting state

### 1.1 What the read side could already answer

Before this change every read route was keyed by one id:

| Route | Subject |
| --- | --- |
| `GET /v1/readings/devices/{id}/fields` | one device, every field |
| `GET /v1/readings/devices/{id}/fields/{field}` | one device, one field |
| `GET /v1/readings/devices/{id}/fields/{field}/history` | one device, one field, over time |
| `GET /v1/readings/groups/{id}/fields` | one group's members, every field |
| `GET /v1/readings/groups/{id}/fields/{field}` | one group's members, one field |
| `GET /v1/readings/groups/{id}/fields/{field}/history` | one group's members, one field, over time |
| `GET /v1/inventory/devices` | every configured device — but *no readings* |

The gap is the bottom two rows read together. `GET /v1/inventory/devices` knows
the fleet and carries no readings; the readings routes carry readings and know
only the id they were handed. A dashboard rendering "every recorder and what it
is doing" had to fetch the inventory, then issue one readings request per device
— an N+1 whose N is the size of the installation, paid on every poll of the
page.

The group routes narrow this and do not close it. A group is a *configured*
set, so "every device in the building" is only expressible if someone wrote that
group into the devices file, and "every device that is stopped" is not
expressible at all: it is not a configured set, it is a predicate over current
state.

### 1.2 What the port would not give

The obvious move is a fleet-wide method on `ReadStore`. It is the wrong move,
and the reason is the same seam the rest of the read side rests on.

`ReadStore` holds what `sismatic-sync` *wrote*. Its own docs say the
consequence out loud: `latest` returning `None` "deliberately conflates 'no such
device', 'no such field' and 'known pair, never yet polled'". A `devices()` on
that port would therefore enumerate *devices that have been polled*, and would
be answering a question about configuration out of a table of observations. The
device installed last week that has never responded — the single most useful row
on a fleet page — is exactly the row such a method cannot produce.

So the port is untouched. The fleet comes from the `DeviceCatalog`, which is the
configured set, and the values come from the store.

## 2. Where the fleet comes from

### 2.1 Catalog for membership, store for values

This is the same division the group read routes already make, applied one level
up. `handlers::group_readings` asks the catalog which devices a group id
addresses and then asks the store what each one reported; `handlers::fleet_readings`
asks the catalog for every device and does the same. A group is a named subset
of the fleet, so the fleet route is the group route with the naming step removed.

### 2.2 The empty row is the finding

A device that is configured and has never answered is a row:

```json
{ "device": "annex-7", "latest": [] }
```

not an omission. This is the whole argument for sourcing membership from the
catalog. Sourced from the store the device would simply be absent, and absent is
indistinguishable from "not installed" — which is the one reading a fleet page
must not silently produce.

### 2.3 The id-space consequence

The per-device readings routes answer an unknown id with an empty list rather
than a `404`, because the store cannot tell a typo from silence and a `404`
would report an unreachable device as an unconfigured one.

The fleet route has no such excuse — it consults the catalog by construction, so
by the time a filter could be applied, the existence of every id in it has
already been settled. An id in `?devices=` that names nothing is therefore a
`404` that names it. The alternative is worse than merely unhelpful: a filter
that silently matched nothing would render a typo as a *healthy, empty fleet*.

This mirrors §2.5 of [[Group-Read-API]] exactly, and for the same reason: a
route that must consult the catalog to answer at all is entitled to the catalog's
claims.

| Id names | `/devices/{id}/fields` | `?devices=<id>` |
| --- | --- | --- |
| a configured device | its readings | its row |
| nothing | `200` with `[]` | `404`, naming the id |
| a device group | `404` → the group *URL* | `404` → the `?group=` *parameter* |

The third row is the one place this route deliberately diverges from
`handlers::target`. Everywhere a group id appears in a *path*, `reject_group`
sends the caller to the route in the other id space. Here the caller is already
on the route that answers for groups — the fix is one parameter over, not one
URL over — so the message says `?group=<id>` rather than pointing at
`/v1/readings/groups/{id}/fields`.

## 3. The two filter axes

### 3.1 Columns and rows

`fields` and `where` are not two spellings of one idea. They cut the answer on
perpendicular axes:

- `?fields=RUNNING_STATE,FIRMWARE` — which **columns** each row carries.
- `?where=RUNNING_STATE:stopped` — which **rows** appear at all.

Both are comma-separated, both fold field names the way a path segment is folded
(`running-state`, `running_state` and `RUNNING_STATE` are one field), and
multiple `where` predicates are conjunctive.

### 3.2 Why a value filter must name its field

The filter could have been spelled `?value=stopped`, and it would have been
ambiguous the moment more than one field was in play: with
`?fields=RUNNING_STATE,FIRMWARE`, a bare value says nothing about which of the
two it constrains. Every predicate therefore carries its own field, and the
`FIELD:value` pairing is what makes `where` composable — a second predicate is
another entry in the same list rather than a second parameter with its own rules.

`:` separates the pair rather than `=` so a value needs no percent-encoding
inside a query string. The list separator is `,`, which a value consequently
cannot contain — see [[#9. Known limits and suboptimal points]].

### 3.3 Selection before projection

The order is load-bearing and is the reason the two filters compose into
something neither can express alone:

```
?fields=FIRMWARE&where=RUNNING_STATE:stopped
```

"the firmware of every stopped recorder." The predicate reads a field the
projection does not keep. Evaluated the other way around — project to
`FIRMWARE`, then filter on `RUNNING_STATE` — the column the predicate needs has
already been dropped and the query matches nothing at all.

So the handler reads each device's *whole* snapshot, evaluates every predicate
against it, and only then keeps the requested columns. The cost is that
`?fields=` narrows the response but not the store read, which is honest: the
store's `latest_all` answers "everything known about this device" in one call and
has no narrower question to be asked.

### 3.4 The comparison was already written

A predicate's value arrives as text in a URL; the store holds a decoded
`ReadingValue`. Reconciling those is exactly what `sismatic_store::group::satisfies`
does for the group routes — it reads the caller's text *in the shape the device
answered in* — so `where` reuses it and introduces no comparison logic of its
own:

| `?where=` | matches a stored |
| --- | --- |
| `RUNNING_STATE:stopped`, `RUNNING_STATE:STOPPED` | `State(Stopped)` |
| `HTTP_PORT:8080` | `Port(8080)` |
| `DHCP_MODE:true`, `:on`, `:1` | `Flag(true)` |
| `FIRMWARE:2.11` | `Version("2.11")` |
| `MAC:00-05-A6-1B-2C-3D` | `Mac(..)`, case-insensitively |

The direction matters: the *reading* is the authority on what shape the field
holds. Nothing in `sismatic-http-api` knows that `HTTP_PORT` is a port, and
nothing has to. That is the same argument that keeps `FieldName` a `String`, one
layer further out. See [[Group-Read-API#4. The comparison]] for the asymmetry
`satisfies` encodes and the direction its errors fall in.

### 3.5 What a predicate cannot distinguish

A device that has never reported the named field holds nothing that could
satisfy the predicate, so it is excluded. `?where=RUNNING_STATE:stopped`
therefore does not separate "not stopped" from "never answered".

This is stated rather than fixed. The unfiltered page shows a silent device as
an empty row, so the distinction is one request away — and the alternative
(a third truth value threaded through a boolean filter) would complicate every
query to serve one.

## 4. Pagination

### 4.1 A page is a whole number of devices

The unit could have been reading rows, matching the `limit` on the history
routes. It is devices instead, because a row is what a client renders: paging by
readings would let one device's snapshot straddle a boundary, and a client would
have to buffer across pages to discover whether it held all of a device or part
of one.

The response is therefore bounded by `limit × fields-per-device` rather than by
a row count. A caller that narrows with `?fields=` pays for what it asked for.

### 4.2 The cursor is a device id

`next` carries the id of the last device on the page, and `after=<that id>`
resumes strictly past it. An opaque token would hide a cursor the caller can
already construct and would have to be decoded somewhere to mean anything.

`next` is always present — `null` on the last page — so a client can loop on
`while next != null` without having to tell "no more pages" from "this server
does not paginate".

### 4.3 The order the cursor indexes

An id cursor is only correct if the sequence it indexes is *totally ordered by
that id and independent of the filters*. This is the one place the
implementation shape is forced.

`DeviceCatalog::devices()` is ordered by id. `DeviceCatalog::group()` returns
members in **configured** order, which is deliberate — the group routes promise
it — and is not sorted. So the handler must not iterate a group's member list
when `?group=` is given; it iterates the catalog's ordered fleet and tests
membership against the group as a *set*. Paging over configured order with an id
cursor would skip and repeat devices.

Every filter is applied the same way, as set membership against one ordered
carrier. That is what makes filter order in the query string irrelevant
([[#8. Algebraic summary]]).

### 4.4 One device past the page

Telling "the page is full" from "the page is full and there is more" needs one
observation beyond the page. The scan therefore continues to the first match
*past* `limit`, sets `next` from the last row it kept, and stops — it does not
filter the whole fleet and then slice.

For an unfiltered `?limit=50` over a thousand recorders that is fifty-one store
reads, not a thousand. Under a selective `?where=` it degrades toward a full scan
([[#9. Known limits and suboptimal points]]).

### 4.5 Zero is refused; too large is clamped

`?limit=0` is a `400`. `?limit=4294967295` is silently clamped to `MAX_LIMIT`.

The asymmetry is not an inconsistency. Clamping still yields a usable page *and*
a cursor that advances, so it costs the caller an extra round trip and nothing
else. A page of zero devices has no last row and so can produce no cursor: a
client that accepted one would loop forever on a `next` that is always `null`,
while the fleet it never saw sits behind it.

| | value |
| --- | --- |
| `DEFAULT_LIMIT` | 100 devices |
| `MAX_LIMIT` | 1 000 devices |

## 5. The wire contract

### 5.1 The types

```rust
pub struct DeviceReadings {
    pub device: DeviceId,
    pub latest: Vec<Reading>,   // ordered by field name; empty if never answered
}

pub struct FleetReadings {
    pub devices: Vec<DeviceReadings>,   // ordered by device id
    pub next: Option<DeviceId>,         // always serialized
}
```

### 5.2 Why `DeviceReadings` is not `DeviceDetail`

`DeviceDetail` already pairs a device with its latest readings, and reusing it
would have been one type fewer. It carries a whole `DeviceSummary` — host, port,
`eager`, and a live `ConnectionStatus` — because it answers an *inventory*
question, and that status is overlaid from the `DeviceStatus` port at request
time.

Reusing it here would drag the status port into the readings scope and blur the
distinction the two scopes are built on: `/v1/readings` reports what was
*written*, `/v1/inventory` reports what was *configured* plus what is live now.
A fleet row is a readings answer, and the id is all of the device that a reading
is filed under.

### 5.3 A worked response

```json
GET /v1/readings/devices?fields=FIRMWARE&where=RUNNING_STATE:stopped&limit=2

{
  "devices": [
    {
      "device": "annex-7",
      "latest": [
        { "device": "annex-7", "field": "FIRMWARE",
          "value": { "type": "version", "value": "2.09" },
          "at": "2026-08-26T09:14:02Z" }
      ]
    },
    {
      "device": "atrium-101",
      "latest": [
        { "device": "atrium-101", "field": "FIRMWARE",
          "value": { "type": "version", "value": "2.11" },
          "at": "2026-08-26T09:14:05Z" }
      ]
    }
  ],
  "next": "atrium-101"
}
```

Both devices are stopped; neither row shows `RUNNING_STATE`, because the
predicate selected rows and `fields` selected columns.

### 5.4 The one parameter whose field name is not its wire name

`where` is a Rust keyword, so `FleetQuery` spells the field `predicates` and
carries `#[serde(rename = "where")]`. `IntoParams` honours the rename, so the
document advertises `where` — and a test pins it, because a client generated
against `predicates` would put its filter in a parameter the server never reads
and would silently receive the whole unfiltered fleet.

## 6. Assembly in the handler

```text
candidates(catalog, query)          catalog fleet, in id order
  ∩ group members        (if ?group=)      404 on an unknown group
  ∩ named devices        (if ?devices=)    404 on an unknown id
  ∩ { id : id > after }  (if ?after=)

for each candidate, in order:
    latest ← store.latest_all(id)          one read per candidate
    if every predicate holds on latest:    ← selection, on the whole snapshot
        if page is full: next ← last kept id; stop
        push { id, project(latest) }       ← projection
```

Three cheap set operations with no I/O, then one store read per surviving
candidate, stopping one match past the page.

## 7. Changes by crate

### 7.1 `sismatic-api-types`

Three DTOs in `reading.rs`: `DeviceReadings`, `FleetReadings`, `FleetQuery`.
`FleetQuery` is separate from `ReadingQuery` rather than an extension of it —
`ReadingQuery` scopes one *series* (one field, one device, a time span) and
carries `start`/`end` and a singular `field`; this scopes a *set of devices at
one instant* and carries no span. Folding them together would document four
parameters on each route that the route ignores.

### 7.2 `sismatic-store`, `sismatic-store-memory`

Unchanged. No new port method and no adapter work — the point of §1.2.

### 7.3 `sismatic-http-api`

New `handlers::fleet_readings`. Reuses `readings::normalize_field` (so a field is
spelled the same way on all four device routes), `target::group_members` (so a
device id in `?group=` is refused with the same message the group routes give
it), and `store::group::satisfies`.

One route registered in `startup`, last among the device routes because it is the
shortest. One operation and one schema added to the OpenAPI document.

### 7.4 `sismatic-server`

Unchanged. `Ports` already carries the catalog and the store; the new route needs
no capability the application was not already assembled over.

## 8. Algebraic summary

### 8.1 The response as relational algebra

Let `F` be the catalog's fleet as a totally ordered carrier, and
`λ : DeviceId → Set(Reading)` the store's `latest_all`. The answer is

```
π_C ∘ σ_P
```

— selection by the predicate set `P`, then projection onto the column set `C`.

The two do **not** commute. `π_C ∘ σ_P = σ_P ∘ π_C` holds only when
`fields(P) ⊆ C`, which is precisely the case `?fields=FIRMWARE&where=RUNNING_STATE:stopped`
violates. Selection is therefore evaluated first, and §3.3 is that
non-commutativity written out in prose.

### 8.2 The filters as a meet-semilattice

The candidate set is

```
F ∩ G ∩ D ∩ A
```

where `G` is the group's members, `D` the named devices, and `A` the cursor's
upper set `{ id : id > after }` — each a subset of `F`, each omitted filter
being `F` itself.

Intersection on `P(F)` is commutative, associative, and idempotent, with `F` as
the identity. Three consequences the route inherits for free:

- filter order in the query string is irrelevant;
- repeating a filter changes nothing;
- omitting one is exactly `∩ F`, which is why "absent" and "do not narrow" are
  the same code path — and why `?fields=` blank must fold to `None` (every
  field) rather than to the empty set (no fields), since those are the *top* and
  *bottom* of the lattice and mean opposite things.

The conjunction of `where` predicates is the same meet one level down:
`σ_{P₁ ∧ P₂} = σ_{P₁} ∘ σ_{P₂}`.

### 8.3 The cursor as a resumable fold

`A = { id : id > after }` is an upper set of a total order, so successive pages
are the blocks of a partition of `σ_P(F)` into consecutive runs, and
`after ← next` is strictly increasing. Termination and exactly-once coverage
follow from the order being total and the step being strict — neither is a
property of the store, which is why the ordering must come from the catalog and
must survive every filter (§4.3).

The walk is idempotent in the sense that matters operationally: re-requesting a
page returns the same devices, since the catalog is fixed for the life of the
process. Only the readings inside a row can change between two requests.

### 8.4 `satisfies` as a relation

Unchanged from [[Group-Read-API#11.4 satisfies as a relation]]: it is reflexive
on typed values, non-symmetric across the `Text`/decoded boundary, and its errors
fall toward false negatives. A `where` predicate is a `Text` expectation on the
left, so a spelling `sismatic-core` accepts and `satisfies` does not excludes a
device that is in fact a match — a missing row rather than a wrong one.

## 9. Known limits and suboptimal points

1. **A `where` value cannot contain a comma.** `,` is the list separator. A
   metadata filter like `TITLE:Lecture 4, Part 2` is unexpressible. Repeated
   `where=` parameters would fix it and are not supported, because
   `serde_urlencoded` — what `web::Query` deserializes with — has no sequence
   support at all, and reaching for repeated params means either a new
   dependency or hand-parsing the raw query string and giving up the derived
   `IntoParams` documentation.

2. **A selective `?where=` degrades to a full scan.** The page is filled by
   reading candidates in order until `limit + 1` match. A predicate that matches
   one device in a thousand reads all thousand. The store cannot be asked a
   narrower question — `ReadStore` is keyed by device — so pushing the predicate
   down would be a port change and, for the in-memory adapter, no faster.

3. **`?fields=` narrows the response, not the read.** `latest_all` returns every
   field; projection happens in the handler. Same cause as (2).

4. **A predicate cannot see a missing field** (§3.5).

5. **The catalog is static per process**, so `next` cannot be invalidated by a
   device appearing mid-walk. This is currently a property of `MemoryCatalog`,
   not a guarantee of the `DeviceCatalog` port. A catalog backed by an inventory
   database would make the cursor's stability a real question — though an id
   cursor degrades far more gracefully there than an offset would, which is part
   of why it was chosen.

6. **No `total`.** Counting the filtered set means evaluating every predicate
   over the whole fleet, which is the cost (2) exists to avoid. A client that
   needs a count can walk the pages.

## 10. Verification

| Layer | What it covers |
| --- | --- |
| `handlers::fleet_readings::tests` | Predicate parsing and matching across value shapes; conjunction; malformed predicates refused; field-name folding; blank-vs-absent filters; projection order; page-size clamping and the zero refusal. No I/O. |
| `tests/readings/fleet.rs` | Black box over the real `MemoryStore` and `MemoryCatalog`: id ordering, the empty row, both filter axes and their composition, `?devices=`/`?group=` intersection, both `404`s and the group-id message, a full walk with `limit=1`, the full-final-page cursor, a cursor over a filtered set, `limit=0`, over-large `limit`, a `500` on backend failure, and `405` on a non-`GET`. |
| `tests/openapi.rs` | The route is served where the document says it is; its six query parameters are documented under their wire names, including the `where` rename. |
| `nix flake check` | fmt, clippy, deny, doc, nextest across the workspace. |

The fleet suite's catalogs are built in the file rather than taken from the
harness: `harness::device_group` puts every device it is given into the one
group, and `?devices=` and `?group=` can only be told apart by a fleet strictly
wider than the group inside it. Hence `beacon`, configured but ungrouped.
