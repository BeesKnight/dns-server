// src/store/auth.ts
import { create } from "zustand";
import { api, User } from "../lib/api";

type AuthState = {
  user: User | null;
  loading: boolean;
  error: string | null;

  login: (email: string, password: string) => Promise<void>;
  register: (email: string, password: string) => Promise<void>;
  logout: () => void;
  bootstrap: () => Promise<void>;
};

export const useAuth = create<AuthState>((set) => ({
  user: null,
  loading: false,
  error: null,

  async login(email, password) {
    set({ loading: true, error: null });
    try {
      const r = await api.login(email, password);
      localStorage.setItem("access_token", r.access_token);
      set({ user: r.user, loading: false });
    } catch (error: unknown) {
      const message = error instanceof Error ? error.message : "Login failed";
      set({ error: message, loading: false });
      throw (error instanceof Error ? error : new Error(message));
    }
  },

  async register(email, password) {
    set({ loading: true, error: null });
    try {
      const r = await api.register(email, password);
      localStorage.setItem("access_token", r.access_token);
      set({ user: r.user, loading: false });
    } catch (error: unknown) {
      const message = error instanceof Error ? error.message : "Register failed";
      set({ error: message, loading: false });
      throw (error instanceof Error ? error : new Error(message));
    }
  },

  logout() {
    localStorage.removeItem("access_token");
    set({ user: null });
  },

  async bootstrap() {
    // есть токен — пробуем подтянуть профиль
    const t = localStorage.getItem("access_token");
    if (!t) {
      set({ user: null });
      return;
    }
    try {
      const me = await api.me();
      set({ user: me });
    } catch {
      // невалидный токен — очистим
      localStorage.removeItem("access_token");
      set({ user: null });
    }
  },
}));
