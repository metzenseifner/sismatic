// src/components/layout/Container.tsx
import styles from "./Container.module.css";

export function Container({children}: {children: React.ReactNode}) {
  return <div className={styles.container}>{children}</div>;
}
