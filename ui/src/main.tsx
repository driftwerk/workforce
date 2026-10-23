import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./index.css";
import "./lib/monaco-setup";
import App from "./App";
import { useThemeStore } from "./stores/theme-store";

// Initialize theme before render to avoid flash
useThemeStore.getState().initialize();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>
);
