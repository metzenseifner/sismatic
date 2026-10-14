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
        // Wrapping is for a *row* that runs out of width. A column has nowhere
        // to wrap to but sideways, and Chrome charges for the possibility: a
        // wrapping column hands each item a main-axis size far larger than its
        // content — measured at 900px, a 569px grid of cards was given 2334px —
        // and a grid item spends the surplus on its own rows, because
        // `align-content`'s initial `normal` behaves as `stretch`. That is what
        // put a thousand pixels between one row of cards and the next.
        flexWrap: direction === "row" ? "wrap" : "nowrap",
        gap: `var(--space-${gap})`,
      }}
    >
      {children}
    </div>
  );
}
