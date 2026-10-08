// src/App.tsx
import {useMemo} from "react";
import {Container} from "./components/layout/Container";
import {Stack} from "./components/layout/Stack";
import {Grid} from "./components/layout/Grid";
import {DeviceCard} from "./components/device/DeviceCard";
import {fromDeviceReads, fromGroupState} from "./components/device/deviceView";
import type {DeviceView} from "./components/device/deviceView";
import {useDevices, useGroups} from "./api/queries";
import type {ApiCallError} from "./api/queries";

export default function App() {
  const devices = useDevices();
  const groups = useGroups();

  // Groups first: a room is the thing an operator looks at, and a device is the
  // thing they drill into. Within each kind, the server's id order is kept so
  // the page does not reshuffle between refetches.
  const views = useMemo<DeviceView[]>(
    () => [
      ...(groups.data ?? []).map(fromGroupState),
      ...(devices.data ?? []).map(fromDeviceReads),
    ],
    [groups.data, devices.data],
  );

  const isLoading = devices.isPending || groups.isPending;
  const error = devices.error ?? groups.error;

  const retry = () => {
    void devices.refetch();
    void groups.refetch();
  };

  return (
    <Container>
      <Stack gap="lg">
        <h1 style={{fontSize: "var(--text-xl)"}}>Sismatic Devices Overview</h1>

        {error && <ErrorNotice error={error} onRetry={retry} />}

        {isLoading && !error && <p>Loading the fleet…</p>}

        {!isLoading && !error && views.length === 0 && (
          <p>
            No devices or groups are configured. Add them to the server's
            devices file and reload its configuration.
          </p>
        )}

        {!error && views.length > 0 && (
          <Grid minItemWidth="22rem">
            {views.map(view => (
              <DeviceCard key={`${view.kind}:${view.id}`} view={view} />
            ))}
          </Grid>
        )}
      </Stack>
    </Container>
  );
}

/**
 * A failed fetch is shown instead of the cards, never alongside them: a stale
 * card that looks live is worse than an empty page, because "is it recording?"
 * is the one question this page exists to answer.
 */
function ErrorNotice({
  error,
  onRetry,
}: {
  error: ApiCallError;
  onRetry: () => void;
}) {
  return (
    <div role="alert">
      <p>
        <strong>Could not reach the Sismatic server.</strong> {error.message}
      </p>
      <button type="button" onClick={onRetry}>
        Try again
      </button>
    </div>
  );
}
