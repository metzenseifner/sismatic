import type {ReactNode} from "react";
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
   * Open on first render. The element owns its state from then on, which is
   * what makes this a *default* rather than a value.
   */
  defaultOpen?: boolean;
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
export function Clapdown({
  title,
  note,
  defaultOpen = false,
  children,
}: ClapdownProps) {
  return (
    <details className={styles.clapdown} open={defaultOpen}>
      <summary className={styles.summary}>
        <span className={styles.title}>{title}</span>
        {note !== undefined && <span className={styles.note}>{note}</span>}
      </summary>
      <div className={styles.body}>{children}</div>
    </details>
  );
}
