import {useState, type ReactNode} from "react";
import styles from "./Clapdown.module.css";

type ClapdownProps = {
  /** The label on the fold, visible open or shut. */
  title: string;
  /**
   * Shown at the far end of the same line. For the one thing that has to be
   * legible while the fold is shut — a count of unsaved edits, say — because
   * shut is the state a card spends its life in.
   */
  note?: ReactNode;
  /**
   * Whether the fold is open — a signal rather than a value.
   *
   * Every time it *changes* the fold snaps to it, which is what lets one
   * control in the bar open or shut every fold on the page at once. Between
   * those moments the fold belongs to whoever is reading it: a card opened by
   * hand stays open while the rest stay shut, until the page-wide setting
   * moves again and takes them all with it.
   *
   * Neither a plain default (which could never be changed from outside after
   * mount) nor fully controlled (which would make a click on the summary do
   * nothing until the page agreed).
   */
  open?: boolean;
  children: ReactNode;
};

/**
 * A titled fold: `<details>` and `<summary>`, styled once.
 *
 * Native rather than a button and a `useState`, which buys the disclosure role,
 * Enter and Space, and a fold that prints open — none of which a
 * reimplementation gets for free, and all of which an operator on a phone
 * notices the absence of.
 */
export function Clapdown({title, note, open = false, children}: ClapdownProps) {
  const [isOpen, setIsOpen] = useState(open);
  // Adjusting state during render on a changed prop, rather than in an effect:
  // an effect would open the fold one paint after the click that asked for it,
  // and forty folds blinking a frame late is visible.
  const [lastSignal, setLastSignal] = useState(open);
  if (lastSignal !== open) {
    setLastSignal(open);
    setIsOpen(open);
  }

  return (
    <details
      className={styles.clapdown}
      open={isOpen}
      // The browser has already toggled the element by the time this fires;
      // this is the state catching up with the DOM, not driving it.
      onToggle={event => setIsOpen(event.currentTarget.open)}
    >
      <summary className={styles.summary}>
        <span className={styles.title}>{title}</span>
        {note !== undefined && <span className={styles.note}>{note}</span>}
      </summary>
      <div className={styles.body}>{children}</div>
    </details>
  );
}
