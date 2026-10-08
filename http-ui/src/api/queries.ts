// React Query hooks over the read side of the HTTP API.
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
import {useQuery, type UseQueryResult} from "@tanstack/react-query";
import {api} from "./client";
import type {ApiError, DeviceReads, GroupFieldStateList} from "./types";

/** How often the fleet pages are refetched. The server polls devices on its own
 *  schedule; this is only how stale a card on screen is allowed to look. */
const REFETCH_MS = 5_000;

/** Guards against a server that kept returning a non-null `next` forever. */
const MAX_PAGES = 100;

export const queryKeys = {
  devices: ["reads", "devices"] as const,
  groups: ["reads", "groups"] as const,
};

/** An API call that answered, but with an error status. */
export class ApiCallError extends Error {
  readonly status: number;
  readonly code: string | null;

  constructor(status: number, body: ApiError | undefined) {
    super(body?.error ?? `Request failed with status ${status}`);
    this.name = "ApiCallError";
    this.status = status;
    this.code = body?.code ?? null;
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
