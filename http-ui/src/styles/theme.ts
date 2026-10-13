// The one piece of JavaScript that knows a mode exists.
//
// It sets an attribute. That is deliberately all it does: `tokens.css` holds
// every colour and every decision about which colour belongs to which mode, so
// nothing here, and nothing in any component, needs to know what dark *looks*
// like. A component asks for `var(--color-surface)` and is right in every mode
// without having been told there are modes.
//
// The division is worth stating plainly, because the temptation is to resolve
// the mode here and hand components a palette:
//
//   - this file says *which mode*, as `data-theme` on the root element;
//   - `tokens.css` says *what that means*, as one mapping of roles per mode;
//   - a component says *which role*, and nothing else.
import {useEffect} from "react";

/** What the operator chose. "system" is the absence of a choice. */
export type ThemeMode = "system" | "light" | "dark";

export const THEME_CHOICES: readonly ThemeMode[] = ["system", "light", "dark"];

export const DEFAULT_THEME: ThemeMode = "system";

/**
 * Put the chosen mode where the stylesheet is waiting for it.
 *
 * "System" *removes* the attribute rather than writing out a resolved value,
 * which is the whole reason this is three lines instead of a `matchMedia`
 * subscription. With no attribute the media query in `tokens.css` decides, so
 * a machine that switches to dark at sunset takes the page with it while this
 * file is asleep — and the page is already correct before the first paint,
 * because CSS got there before React did.
 *
 * An effect rather than a render-time write: setting an attribute on
 * `document` is exactly the "synchronise with something outside React" that
 * effects are for, and doing it during render would make a component that
 * renders twice write twice.
 */
export function useThemeMode(mode: ThemeMode): void {
  useEffect(() => {
    const root = document.documentElement;
    if (mode === "system") delete root.dataset.theme;
    else root.dataset.theme = mode;
  }, [mode]);
}
