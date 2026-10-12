// src/components/layout/Grid.tsx
import styles from "./Grid.module.css";

/** How many cards go in a row: a number, or let the width decide. */
export type Columns = "auto" | number;

/**
 * The narrowest a card may be squeezed before a column is dropped instead.
 *
 * Low on purpose, and tuned rather than guessed. Six cards across a 72rem page
 * come to about 10rem each, which is cramped — and is exactly what "six per
 * row" asks for, so it is granted. The figure has to sit under that with room
 * to spare, because `--space-lg` keeps growing with the viewport after
 * `--max-width` has stopped: the gaps on a 120rem monitor are wider than those
 * on a 72rem one while the content is no wider, so the same six columns have
 * slightly less to share. At 10rem that tipped over and a wide monitor showed
 * five where a smaller window showed six; 9rem holds six from 72rem upwards.
 *
 * What keeps any of this off a phone is not this number but the width the
 * stylesheet demands before it honours a count at all. Between the two, a
 * narrow desktop asking for six gets five, rather than six unreadable slivers.
 */
const CARD_FLOOR = "9rem";

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
