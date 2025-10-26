import { useEffect, useSyncExternalStore } from "react";

import { api, type GeoInfo } from "../lib/api";

type ConnectionState = {
  ip: string | null;
  geo: GeoInfo | null;
  loading: boolean;
  error: Error | null;
};

type Listener = () => void;

let state: ConnectionState = {
  ip: null,
  geo: null,
  loading: false,
  error: null,
};

const listeners = new Set<Listener>();
let initialized = false;
let inFlight: Promise<void> | null = null;

const notify = () => {
  for (const listener of listeners) {
    listener();
  }
};

const setState = (patch: Partial<ConnectionState>) => {
  state = { ...state, ...patch };
  notify();
};

const subscribe = (listener: Listener) => {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
};

const getSnapshot = () => state;

const fetchConnection = async () => {
  if (inFlight) return inFlight;

  const promise = (async () => {
    setState({ loading: true, error: null });
    try {
      const user = await api.me();
      const currentIp = user.current_ip ?? null;
      setState({ ip: currentIp, geo: null });

      if (currentIp) {
        try {
          const geo = await api.geoLookup(currentIp);
          setState({ geo });
        } catch (geoError) {
          const err = geoError instanceof Error ? geoError : new Error("Geo lookup failed");
          setState({ error: err, geo: null });
        }
      }
    } catch (error) {
      const err = error instanceof Error ? error : new Error("Failed to load connection info");
      setState({ error: err, geo: null });
    } finally {
      setState({ loading: false });
      inFlight = null;
    }
  })();

  inFlight = promise;
  return promise;
};

export const refreshConnection = () => {
  if (state.loading) return inFlight;
  return fetchConnection();
};

export function useConnection() {
  const snapshot = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);

  useEffect(() => {
    if (!initialized) {
      initialized = true;
      void fetchConnection();
    } else if (!state.ip && !state.geo && !state.loading && !state.error) {
      void fetchConnection();
    }
  }, []);

  return { ...snapshot, refresh: refreshConnection };
}

