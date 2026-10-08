// src/components/layout/Stack.tsx
type StackProps = {
  children: React.ReactNode;
  direction?: "row" | "column";
  gap?: "sm" | "md" | "lg";
};

export function Stack({
  children,
  direction = "column",
  gap = "md",
}: StackProps) {
  return (
    <div
      style={{
        display: "flex",
        flexDirection: direction,
        flexWrap: "wrap",
        gap: `var(--space-${gap})`,
      }}
    >
      {children}
    </div>
  );
}
