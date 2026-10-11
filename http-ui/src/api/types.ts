// Short names for the schema types the UI actually touches. Generated
// `schema.d.ts` spells every type as `components["schemas"]["X"]`, which is
// unreadable in a signature and couples each use site to the generator's
// shape; one alias per type keeps `pnpm gen:api` a drop-in replacement.
import type {components} from "./schema";

type Schemas = components["schemas"];

/** One stored read: device `field` held `value` as of `at`. */
export type Read = Schemas["Read"];
/** The decoded form of a read — a discriminated union on `type`. */
export type ReadValue = Schemas["ReadValue"];
/** One device's latest read of each field. Empty `latest` means it never answered. */
export type DeviceReads = Schemas["DeviceReads"];
/** A page of the fleet's latest reads, one row per device. */
export type FleetReads = Schemas["FleetReads"];
/** Every field one group knows about, with each member's latest value. */
export type GroupFieldStateList = Schemas["GroupFieldStateList"];
/** A page of group states, one row per group. */
export type FleetGroupReads = Schemas["FleetGroupReads"];
/** Whether members hold what was asked of them. */
export type GroupSyncState = Schemas["GroupSyncState"];
/** What the server has been *told* to hold, as opposed to what a device reported. */
export type DesiredRecordingState = Schemas["DesiredRecordingState"];
/** The error body every failing route returns. */
export type ApiError = Schemas["ApiError"];
/** Everything a write can name, in three lists — the body of `GET /v1/writes`. */
export type WritesCatalog = Schemas["WritesCatalog"];
/** One catalog entry: the canonical `name`, its `aliases`, and a one-line `description`. */
export type InstructionSummary = Schemas["InstructionSummary"];
/** The `202 Accepted` body: one entry per device the submission produced. */
export type Acceptance = Schemas["Acceptance"];
/** Which precondition refused a write. Every one of these arrives as a `409`. */
export type Rejection = Schemas["Rejection"];
