// The recorded data the pages replay. The build writes each as an ES module (assets/<id>/data/<name>.js, an
// `export default` of the JSON in content/data/<name>.json), so these declarations are all tsc sees of them.

interface TraceData {
  schema: number;
  duration_ms: number;
  nodes: number;
  client: { cached: number; commands: number; failed: number; local: number; remote: number; pending: [number, number][] };
  /** [daemon, start ms, end ms, outputs flag]: a lease. */
  grants: [number, number, number, number][];
  /** [daemon, end ms, failed flag] */
  remote: [number, number, number][];
  /** [ms, count] */
  hits: [number, number][];
}

interface CastData {
  schema: number;
  command: string;
  outcome: string;
  editor: { path: string; buck: string; bazel: string };
  /** [ms, text] */
  lines: [number, string][];
}

interface MemRung {
  rung_gib: number;
  actions: number;
  booked_mib: number;
  used_mib: number;
  idle_mib: number;
  kills: number;
}

interface MemData {
  schema: number;
  actions: number;
  ok: number;
  oom: number;
  peaks_mib: number[];
  percentiles_mib: { max: number; p50: number; p90: number; p99: number };
  model: {
    beyond_top: number;
    slot_gib: number;
    slot: { booked_mib: number; fit: number; idle_mib: number; killed: number; used_mib: number };
    ladder: MemRung[];
  };
}

declare module '*/data/trace.js' {
  const data: TraceData;
  export default data;
}
declare module '*/data/cast.js' {
  const data: CastData;
  export default data;
}
declare module '*/data/mem.js' {
  const data: MemData;
  export default data;
}
