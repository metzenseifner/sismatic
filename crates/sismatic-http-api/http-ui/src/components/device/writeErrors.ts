// A refused write, in the words an operator can act on.
//
// `ApiError` carries two machine-readable fields rather than one, and this is
// the file that spends the second: `code` says what kind of failure it was and
// fixes the status, `rejection` says which precondition refused it and is
// always a 409. Matching on `rejection` is what lets a card tell a change that
// was *discarded* from a fleet that was already doing what it was asked —
// without that distinction a client either treats every 409 as a failure or
// silently loses metadata.
import type {ApiCallError} from "../../api/queries";

/**
 * Whether the fleet was already in the state that was asked for.
 *
 * Three of the five rejections say exactly that, and they are not failures: a
 * second operator hitting Stop on an already-stopped recorder has not broken
 * anything, and showing them a red alert for it teaches them to ignore red
 * alerts. They are shown as a note instead.
 */
export function isBenign(error: ApiCallError): boolean {
  return (
    error.rejection === "already_recording" ||
    error.rejection === "already_paused" ||
    error.rejection === "not_recording"
  );
}

/** One sentence saying what happened and what to do about it. */
export function writeErrorText(error: ApiCallError): string {
  switch (error.rejection) {
    case "metadata_frozen":
      return "A recording is in progress, so the registers are sealed. Stop it, or write them before the next one starts.";
    case "already_recording":
      return "That recording had already started.";
    case "already_paused":
      return "That recording was already paused.";
    case "not_recording":
      return "Nothing is recording.";
    // Configuration, not a state: this one will refuse forever, so there is
    // nothing to be gained by trying again.
    case "field_disabled":
      return `${error.message} It stays refused until the field is re-enabled in the server's devices file.`;
    default:
      return error.message;
  }
}
