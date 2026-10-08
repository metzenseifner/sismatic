import {StrictMode} from "react";
import {createRoot} from "react-dom/client";
import {QueryClient, QueryClientProvider} from "@tanstack/react-query";
import "./styles/global.css";
import App from "./App.tsx";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // The fleet hooks refetch on their own interval, so the extra refetch on
      // every window focus is noise.
      refetchOnWindowFocus: false,
      retry: 1,
    },
  },
});

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <App />
    </QueryClientProvider>
  </StrictMode>,
);
