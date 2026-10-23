// src/App.tsx
import {useMemo} from "react";
import {Container} from "./components/layout/Container";
import {Stack} from "./components/layout/Stack";
import {Grid} from "./components/layout/Grid";
import {ControlBar} from "./components/fleet/ControlBar";
import {applyFilters, searchable, NO_FILTERS} from "./components/fleet/fleetFilters";
import {useFleetSettings} from "./components/fleet/fleetSettings";
import {useThemeMode} from "./styles/theme";
import {LiveDeviceCard} from "./components/device/LiveDeviceCard";
import {fromDeviceReads, fromGroupState} from "./components/device/deviceView";
import type {DeviceView} from "./components/device/deviceView";
import {useDevices, useGroups, useInventoryGroups} from "./api/queries";
import type {ApiCallError} from "./api/queries";

export default function App() {
  // The page's own state, held here because it is the one place that both the
  // control bar and the things it controls can be reached from: the interval
  // parameterises the queries, the columns go to the grid, and the filters sit
  // between the two. Restored from the last visit — see `useFleetSettings`.
  const [settings, setSettings] = useFleetSettings();
  const {filters, display, pollMs, columns, controls} = settings;

  // The one line of theming outside `styles/`. Nothing below this point —
  // no card, no badge, no fold — is aware that modes exist.
  useThemeMode(settings.theme);

  const devices = useDevices(pollMs);
  const groups = useGroups(pollMs);
  // Membership, which the read routes cannot give for a group that has never
  // answered. Deliberately not folded into `error` below: a card that knows
  // what is recording but not who is in the group is worth far more than no
  // page at all, so a failure here degrades to the reads instead of blanking
  // the fleet.
  const inventory = useInventoryGroups(pollMs);

  const membersByGroup = useMemo(
    () => new Map((inventory.data ?? []).map(g => [g.id, g.members])),
    [inventory.data],
  );

  // Groups first: a room is the thing an operator looks at, and a device is the
  // thing they drill into. Within each kind, the server's id order is kept so
  // the page does not reshuffle between refetches.
  const views = useMemo<DeviceView[]>(
    () => [
      ...(groups.data ?? []).map(group =>
        fromGroupState(group, membersByGroup.get(group.group)),
      ),
      ...(devices.data ?? []).map(fromDeviceReads),
    ],
    [groups.data, devices.data, membersByGroup],
  );

  // Two memos, not one: the fields a text search reads are rebuilt when the
  // fleet changes, the filtering is redone on every keystroke, and collapsing
  // them would do the first work at the rate of the second.
  const rows = useMemo(() => searchable(views), [views]);
  const visible = useMemo(() => applyFilters(rows, filters), [rows, filters]);

  const isLoading = devices.isPending || groups.isPending;
  const error = devices.error ?? groups.error;

  const retry = () => {
    void devices.refetch();
    void groups.refetch();
  };

  return (
    <Container>
      <h1
        style={{
          fontSize: "var(--text-xl)",
          marginBlock: "var(--space-lg) var(--space-md)",
        }}
      >
        Sismatic Fleet Overview
      </h1>

      {/* Inside the container, beside the grid rather than above the page, so
          that the bar is exactly as wide as the cards it controls. */}
      <ControlBar
        settings={settings}
        onChange={setSettings}
        shown={visible.length}
        total={views.length}
      />

      <Stack gap="lg">
        {error && <ErrorNotice error={error} onRetry={retry} />}

        {isLoading && !error && <p>Loading the fleet…</p>}

        {!isLoading && !error && views.length === 0 && (
          <p>
            No devices or groups are configured. Add them to the server's
            devices file and reload its configuration.
          </p>
        )}

        {/* Kept apart from the message above, which would otherwise send an
            operator to edit a configuration file over a filter they typed
            themselves thirty seconds ago. */}
        {!error && views.length > 0 && visible.length === 0 && (
          <p>
            Nothing matches these filters.{" "}
            <button
              type="button"
              onClick={() => setSettings({...settings, filters: NO_FILTERS})}
            >
              Clear filters
            </button>
          </p>
        )}

        {!error && visible.length > 0 && (
          <Grid minItemWidth="22rem" columns={columns}>
            {visible.map(view => (
              <LiveDeviceCard
                key={`${view.kind}:${view.id}`}
                view={view}
                display={display}
                controls={controls}
              />
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
