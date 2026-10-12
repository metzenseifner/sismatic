// src/components/layout/Masonry.tsx
import {
  Children,
  useLayoutEffect,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
} from "react";
import styles from "./Masonry.module.css";

type MasonryProps = {
  children: ReactNode;
  /** The narrowest a card is allowed to get before a column is dropped. */
  minItemWidth?: string;
};

/**
 * Cards laid out in columns that do not share row heights.
 *
 * Reading order is the grid's: with *n* columns, item *i* goes to column
 * *i mod n*, which is the track a `repeat(auto-fill, …)` grid would have put it
 * in. What changes is only what happens underneath — see `Masonry.module.css`
 * for why a grid cannot leave that alone.
 *
 * The columns are real elements, because they have to be: nothing in CSS
 * detaches one grid column's rows from the next one's. The cost is that a card
 * changes parent when the column count changes, so a resize across a
 * breakpoint remounts the cards and takes their local state — an open fold,
 * text typed into a register box — with it. That is a window being dragged
 * between widths, not anything an operator does mid-take, and the alternative
 * is absolute positioning, which would have to measure every card on every
 * change and would animate none of it.
 */
export function Masonry({children, minItemWidth = "16rem"}: MasonryProps) {
  const host = useRef<HTMLDivElement>(null);
  const [columnCount, setColumnCount] = useState(1);

  // Before paint, so the first frame is not a single column: the track list is
  // CSS's and does not depend on how many columns are rendered into it, so one
  // measurement from the one-column first pass already gives the final answer.
  useLayoutEffect(() => {
    const element = host.current;
    if (element === null) return;

    const measure = () => {
      // `auto-fill` creates every track that fits whether or not anything is
      // in it, and the resolved value is a list of pixel lengths, so counting
      // them is CSS's own answer to "how many columns" read back. The `px`
      // filter is for the one case where it is not resolved — an element with
      // no layout box returns the declared `repeat(…)` instead.
      const count = getComputedStyle(element)
        .gridTemplateColumns.split(" ")
        .filter(track => track.endsWith("px")).length;
      setColumnCount(Math.max(1, count));
    };

    measure();

    // Width only. The observer fires for height too, which includes every
    // frame of a fold opening, and re-reading the tracks there would be a
    // style flush per frame for an answer that cannot have changed.
    let width = element.getBoundingClientRect().width;
    const observer = new ResizeObserver(entries => {
      const next = entries[entries.length - 1].contentRect.width;
      if (next === width) return;
      width = next;
      measure();
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const items = Children.toArray(children);
  // Never more columns than there are cards: an empty column is an empty grid
  // track either way, so this only keeps empty elements out of the document.
  const count = Math.max(1, Math.min(columnCount, items.length));
  const columns: ReactNode[][] = Array.from({length: count}, () => []);
  items.forEach((item, index) => columns[index % count].push(item));

  return (
    <div
      ref={host}
      className={styles.masonry}
      style={{"--masonry-min": minItemWidth} as CSSProperties}
    >
      {columns.map((column, index) => (
        <div key={index} className={styles.column}>
          {column}
        </div>
      ))}
    </div>
  );
}
