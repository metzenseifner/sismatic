// src/components/layout/Grid.tsx
export function Grid({
  children,
  minItemWidth = "16rem",
}: {
  children: React.ReactNode;
  minItemWidth?: string;
}) {
  return (
    <div
      style={{
        display: "grid",
        gridTemplateColumns: `repeat(auto-fill, minmax(min(${minItemWidth}, 100%), 1fr))`,
        gap: "var(--space-lg)",
      }}
    >
      {children}
    </div>
  );
}
