// src/components/ui/Card.tsx
import styles from "./Card.module.css";

export function Card({
  title,
  image,
  children,
}: {
  title: string;
  image?: string;
  children: React.ReactNode;
}) {
  return (
    <article className={styles.wrapper}>
      <div className={styles.card}>
        {image && <img src={image} alt="" className={styles.image} />}
        <div>
          <h3 className={styles.title}>{title}</h3>
          {children}
        </div>
      </div>
    </article>
  );
}
