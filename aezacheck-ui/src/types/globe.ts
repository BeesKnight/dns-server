export type Arc = {
  id: string;
  startLat: number;
  startLng: number;
  endLat: number;
  endLng: number;
  color: string;
  status: "running" | "ok" | "fail";
};

export type Dot = {
  id: string;
  lat: number;
  lng: number;
  color: string;
  size: number;
  label?: string;
};

export type WorkerCommand =
  | {
      type: "agent.online";
      data: {
        id: string;
        ip: string;
        geo: [number, number];
      };
    }
  | {
      type: "check.start";
      data: {
        check_id: string;
        source: { geo: [number, number] };
        target: { geo: [number, number]; host: string };
      };
    }
  | {
      type: "check.done";
      data: {
        check_id: string;
        ok: boolean;
      };
    }
  | {
      type: "points.remove";
      data: {
        ids: string[];
      };
    };

export type WorkerResponse =
  | { type: "points:add"; points: Dot[] }
  | { type: "points:update"; points: Dot[] }
  | { type: "points:remove"; ids: string[] }
  | { type: "arcs:add"; arcs: Arc[] }
  | { type: "arcs:update"; arcs: Arc[] }
  | { type: "arcs:remove"; ids: string[] };
