// React Query hooks over the HTTP API: the fleet reads, the write catalog, and
// the two kinds of write a card can submit.
//
// Two things are unwrapped here so that no component ever has to:
//
//  1. `openapi-fetch` returns `{data, error}` rather than throwing, but React
//     Query decides "loaded" vs "failed" by whether the query function threw.
//  2. Both fleet routes paginate. `next` is always present — the id of the last
//     row on the page, or `null` on the last one — so a client loops on
//     `while next != null` without having to tell "no more pages" from "this
//     server does not paginate". Showing only the first page would quietly drop
//     devices once the fleet outgrows the server's default page size, which is
//     exactly the failure an overview page must not have.
import {
  useMutation,
  useQuery,
  useQueryClient,
  type QueryClient,
  type UseMutationResult,
  type UseQueryResult,
} from "@tanstack/react-query";
import {api} from "./client";
import type {
  Acceptance,
  ApiError,
  DeviceReads,
  GroupFieldStateList,
  Rejection,
  WritesCatalog,
} from "./types";

/** How often the fleet pages are refetched. The server polls devices on its own
 *  schedule; this is only how stale a card on screen is allowed to look. */
const REFETCH_MS = 5_000;

/** Guards against a server that kept returning a non-null `next` forever. */
const MAX_PAGES = 100;

export const queryKeys = {
  devices: ["reads", "devices"] as const,
  groups: ["reads", "groups"] as const,
  writesCatalog: ["writes", "catalog"] as const,
};

/** An API call that answered, but with an error status. */
export class ApiCallError extends Error {
  readonly status: number;
  readonly code: string | null;
  /**
   * Which precondition refused a write, for the `409`s the write routes return.
   *
   * A second field rather than more `code` variants because the two are
   * different questions — "what kind of failure" and "which rule" — and every
   * rejection shares one status. Branching on this is what lets a card tell an
   * edit that was *discarded* from a fleet that was already doing what was
   * asked; see `writeErrorText`.
   */
  readonly rejection: Rejection | null;

  constructor(status: number, body: ApiError | undefined) {
    super(body?.error ?? `Request failed with status ${status}`);
    this.name = "ApiCallError";
    this.status = status;
    this.code = body?.code ?? null;
    this.rejection = body?.rejection ?? null;
  }
}

/**
 * Every configured device's latest values, one row per device, ordered by id.
 *
 * A device that has never answered is a row with an empty `latest` rather than
 * an absent one — that row is the point, so it is passed through untouched.
 */
export function useDevices(): UseQueryResult<DeviceReads[], ApiCallError> {
  return useQuery({
    queryKey: queryKeys.devices,
    refetchInterval: REFETCH_MS,
    queryFn: ({signal}) =>
      collectPages(signal, async after => {
        const {data, error, response} = await api.GET("/v1/reads/devices", {
          signal,
          params: {query: after === null ? {} : {after}},
        });
        if (error || !data) throw new ApiCallError(response.status, error);
        return {rows: data.devices, next: data.next ?? null};
      }),
  });
}

/** Every configured device group's state, one row per group, ordered by id. */
export function useGroups(): UseQueryResult<
  GroupFieldStateList[],
  ApiCallError
> {
  return useQuery({
    queryKey: queryKeys.groups,
    refetchInterval: REFETCH_MS,
    queryFn: ({signal}) =>
      collectPages(signal, async after => {
        const {data, error, response} = await api.GET("/v1/reads/groups", {
          signal,
          params: {query: after === null ? {} : {after}},
        });
        if (error || !data) throw new ApiCallError(response.status, error);
        return {rows: data.groups, next: data.next ?? null};
      }),
  });
}

/** Walks `next` to the last page and concatenates the rows, in page order. */
async function collectPages<T>(
  signal: AbortSignal,
  fetchPage: (after: string | null) => Promise<{rows: T[]; next: string | null}>,
): Promise<T[]> {
  const rows: T[] = [];
  let after: string | null = null;

  for (let page = 0; page < MAX_PAGES; page++) {
    const {rows: pageRows, next} = await fetchPage(after);
    rows.push(...pageRows);
    // An empty page with a non-null `next` would otherwise spin to MAX_PAGES.
    if (next === null || pageRows.length === 0) return rows;
    signal.throwIfAborted();
    after = next;
  }

  return rows;
}

/**
 * Every command, metadata register, and setting a write can name.
 *
 * Fetched once and never refetched: the three lists are two enums compiled into
 * the server, so unlike the fleet reads beside them they cannot change while
 * this page is open. The `metadata` list is what the Registers panel draws its
 * rows from — a register added to `sismatic-core` appears there with no change
 * here, which is the whole point of the route being a catalog.
 */
export function useWritesCatalog(): UseQueryResult<WritesCatalog, ApiCallError> {
  return useQuery({
    queryKey: queryKeys.writesCatalog,
    staleTime: Infinity,
    gcTime: Infinity,
    queryFn: async ({signal}) => {
      // This route declares only a `200`, so the generated types leave no error
      // branch to narrow into — which is why the status is read off the
      // response before the guard rather than inside it. The guard stays
      // anyway: an auth middleware or a proxy in front of the server can
      // answer something the document does not describe.
      const {data, response} = await api.GET("/v1/writes", {signal});
      const {status} = response;
      if (!data) throw new ApiCallError(status, undefined);
      return data;
    },
  });
}

// ─── Writes ──────────────────────────────────────────────────────────────────

/**
 * Which of the two URL spaces under `/v1/writes` a write is addressed to.
 *
 * Devices and groups share one id namespace and each space accepts only its own
 * kind — a group id under `/v1/writes/devices` is a `404`, not a fan-out — so
 * the kind travels with the id rather than being guessed from it.
 */
export type WriteTarget = {kind: "device" | "group"; id: string};

/** The recording lifecycle, one verb per route. */
export type RecordingVerb = "start" | "stop" | "pause";

/** One register to write, and what to write into it. An empty `value` clears it. */
export type RegisterWrite = {field: string; value: string};

// Spelled out rather than built by string concatenation: `openapi-fetch` types
// each call against the literal path, so these are what makes a typo here a
// build error instead of a 404 at 3am.
const RECORDING_ROUTES = {
  device: {
    start: "/v1/writes/devices/{id}/recording/start",
    stop: "/v1/writes/devices/{id}/recording/stop",
    pause: "/v1/writes/devices/{id}/recording/pause",
  },
  group: {
    start: "/v1/writes/groups/{id}/recording/start",
    stop: "/v1/writes/groups/{id}/recording/stop",
    pause: "/v1/writes/groups/{id}/recording/pause",
  },
} as const;

const METADATA_ROUTES = {
  device: "/v1/writes/devices/{id}/metadata/{field}",
  group: "/v1/writes/groups/{id}/metadata/{field}",
} as const;

/**
 * Start, stop, or pause a recording on one device or one group.
 *
 * The `202` this resolves with means the write is in the outbox, not that any
 * device has been reached — nothing under `/v1/writes` touches a device. So the
 * recording badge, which shows what a device last *reported*, keeps saying the
 * old thing for as long as the relay takes; `LiveDeviceCard` is where that gap
 * is made visible rather than papered over.
 */
export function useRecordingCommand(
  target: WriteTarget,
): UseMutationResult<Acceptance, ApiCallError, RecordingVerb> {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: async (verb: RecordingVerb) => {
      const {data, error, response} = await api.POST(
        RECORDING_ROUTES[target.kind][verb],
        {params: {path: {id: target.id}}},
      );
      if (error || !data) throw new ApiCallError(response.status, error);
      return data;
    },
    onSuccess: () => invalidateFleet(queryClient),
  });
}

/**
 * Write metadata registers, in the order given.
 *
 * Sequential rather than `Promise.all`: each write is admitted against the
 * desired recording state the server holds when it arrives, so a start racing
 * them would refuse an unpredictable subset instead of a suffix. A failure
 * part-way therefore leaves the writes before it already accepted — the caller
 * keeps its drafts and may submit again, which costs a repeated write of a
 * register that already holds that value and nothing else.
 */
export function useMetadataWrites(
  target: WriteTarget,
): UseMutationResult<Acceptance[], ApiCallError, RegisterWrite[]> {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: async (writes: RegisterWrite[]) => {
      const accepted: Acceptance[] = [];
      for (const write of writes) {
        const {data, error, response} = await api.PUT(
          METADATA_ROUTES[target.kind],
          {
            params: {path: {id: target.id, field: write.field}},
            body: {value: write.value},
          },
        );
        if (error || !data) throw new ApiCallError(response.status, error);
        accepted.push(data);
      }
      return accepted;
    },
    onSuccess: () => invalidateFleet(queryClient),
  });
}

/**
 * Refetch both fleet pages after a write.
 *
 * Worth doing even though no device has been contacted yet: a group's row
 * carries `desired_recording_state`, which moves the moment a start is
 * accepted, so this is what makes "Requested state" under Details answer
 * immediately instead of on the next five-second tick.
 */
function invalidateFleet(queryClient: QueryClient): void {
  void queryClient.invalidateQueries({queryKey: queryKeys.devices});
  void queryClient.invalidateQueries({queryKey: queryKeys.groups});
}
