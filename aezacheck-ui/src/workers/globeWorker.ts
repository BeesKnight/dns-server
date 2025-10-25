/// <reference lib="webworker" />

import type { Arc, Dot, WorkerCommand, WorkerResponse } from "../types/globe";

const arcStore = new Map<string, Arc>();
const pointStore = new Map<string, Dot>();

const COLORS = {
  agent: "#59a5ff",
  arc: "#9b5de5",
  target: "#ffe66d",
  ok: "#4ade80",
  fail: "#ef4444",
};

const ctx: DedicatedWorkerGlobalScope = self as any;

ctx.addEventListener("message", (event: MessageEvent<WorkerCommand>) => {
  const message = event.data;

  switch (message.type) {
    case "agent.online": {
      const { id, ip, geo } = message.data;
      const [lat, lng] = geo;
      const dot: Dot = {
        id: `ag:${id}`,
        lat,
        lng,
        color: COLORS.agent,
        size: 0.4,
        label: `agent ${ip}`,
      };
      pointStore.set(dot.id, dot);
      emit({ type: "points:update", points: [dot] });
      break;
    }

    case "check.start": {
      const { source, target, check_id } = message.data;
      const [slat, slng] = source.geo;
      const [tlat, tlng] = target.geo;

      const arc: Arc = {
        id: check_id,
        startLat: slat,
        startLng: slng,
        endLat: tlat,
        endLng: tlng,
        color: COLORS.arc,
        status: "running",
      };
      arcStore.set(arc.id, arc);
      emit({ type: "arcs:add", arcs: [arc] });

      const startPoint: Dot = {
        id: `s:${check_id}`,
        lat: slat,
        lng: slng,
        color: COLORS.arc,
        size: 0.5,
        label: "agent",
      };
      const targetPoint: Dot = {
        id: `t:${check_id}`,
        lat: tlat,
        lng: tlng,
        color: COLORS.target,
        size: 0.5,
        label: target.host,
      };

      pointStore.set(startPoint.id, startPoint);
      pointStore.set(targetPoint.id, targetPoint);
      emit({ type: "points:add", points: [startPoint, targetPoint] });
      break;
    }

    case "check.done": {
      const { check_id, ok } = message.data;
      const arc = arcStore.get(check_id);
      if (arc) {
        const updated: Arc = {
          ...arc,
          color: ok ? COLORS.ok : COLORS.fail,
          status: ok ? "ok" : "fail",
        };
        arcStore.set(check_id, updated);
        emit({ type: "arcs:update", arcs: [updated] });
      }
      break;
    }

    case "points.remove": {
      const { ids } = message.data;
      const removed: string[] = [];
      ids.forEach((id) => {
        if (pointStore.delete(id)) {
          removed.push(id);
        }
      });
      if (removed.length) {
        emit({ type: "points:remove", ids: removed });
      }
      break;
    }

    default:
      break;
  }
});

function emit(payload: WorkerResponse) {
  ctx.postMessage(payload);
}

export type { Arc, Dot, WorkerCommand, WorkerResponse };
