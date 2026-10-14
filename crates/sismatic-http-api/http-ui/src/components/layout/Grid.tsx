// src/components/layout/Grid.tsx
import styles from "./Grid.module.css";

/** How many cards go in a row: a number, or let the width decide. */
export type Columns = "auto" | number;

/**
 * The narrowest a card may be squeezed before a column is dropped instead.
 *
 * Measured against the card, not against a column count anyone wanted. On a
 * 72rem page the cards come out 544px at two per row, 355px at three, 260px at
 * four, and 165px at six; the control row stops fitting on one line below
 * about 22rem, and by 10rem a 165px card is trying to hold a "Recording" badge
 * and a "Stop recording" button in 121px of usable width. 16rem is the far
 * side of that — tight, still legible.
 *
 * This figure decides the column count, and not the other way round. An
 * earlier version of it was picked by working backwards from "make six fit at
 * 72rem", which is how it came to be 9rem and how six per row came to mean six
 * unreadable slivers. A floor exists to say how narrow a card may get; what
 * then fits is arithmetic.
 *
 * So a requested count is a ceiling and never a promise — `Up to 4` is what the
 * menu says — and on a 72rem container the honest answer to five or six is
 * four. Raising `--max-width` is what makes the larger counts mean anything;
 * six needs about 106rem of container before this floor will allow it.
 *
 * What keeps any of this off a phone is not this number but the width the
 * stylesheet demands before it honours a count at all. See the media query in
 * `Grid.module.css`.
 */
const CARD_FLOOR = "16rem";

export function Grid({
  children,
  minItemWidth = "16rem",
  columns = "auto",
}: {
  children: React.ReactNode;
  /** The minimum card width used wherever the count does not apply. */
  minItemWidth?: string;
  columns?: Columns;
}) {
  const style: Record<string, string> = {"--grid-min": minItemWidth};

  if (columns !== "auto") {
    style["--grid-floor"] = CARD_FLOOR;
    // What one of n columns comes to, gaps taken out first — less a pixel.
    //
    // The pixel is not cosmetic. `auto-fill` repeats a track while
    // `floor((available + gap) / (track + gap))` allows another, and asking for
    // the width that divides exactly puts that quotient precisely on n: round
    // the track up by a hundredth of a pixel anywhere in layout and the floor
    // drops to n - 1, so a request for six columns would render as five, at
    // some window widths and not others. Shaving a pixel moves the quotient
    // just above n and the `1fr` maximum stretches the tracks back out, so the
    // row is n equal columns filling the width, reliably.
    style["--grid-exact"] =
      `calc((100% - ${columns - 1} * var(--grid-gap)) / ${columns} - 1px)`;
  }

  return (
    <div
      className={
        columns === "auto" ? styles.grid : `${styles.grid} ${styles.capped}`
      }
      style={style as React.CSSProperties}
    >
      {children}
    </div>
  );
}
