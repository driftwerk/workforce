import { create } from "zustand";

type Theme = "dark" | "light";
const THEME_KEY = "workforce-theme";

interface ThemeState {
  theme: Theme;
  toggle: () => void;
  initialize: () => void;
}

export const useThemeStore = create<ThemeState>((set) => ({
  theme: "dark",

  initialize: () => {
    const stored = localStorage.getItem(THEME_KEY) as Theme | null;
    const theme = stored === "light" ? "light" : "dark";
    document.documentElement.classList.toggle("dark", theme === "dark");
    set({ theme });
  },

  toggle: () => {
    set((state) => {
      const next = state.theme === "dark" ? "light" : "dark";
      document.documentElement.classList.toggle("dark", next === "dark");
      localStorage.setItem(THEME_KEY, next);
      return { theme: next };
    });
  },
}));
