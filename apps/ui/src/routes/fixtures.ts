// Test fixtures shared by the screen tests (not imported by the app).
import type { MemberDto, ServiceDto, StatusDto } from "../lib/api";

export function member(over: Partial<MemberDto> & { id: string; name: string }): MemberDto {
  return {
    role: "host",
    platform: "windows",
    tags: [],
    isSelf: false,
    effective: true,
    online: true,
    rttMs: 12,
    lastSeenMs: null,
    lag: 0,
    accentHue: 200,
    version: null,
    incompatible: false,
    ...over,
  };
}

export function running(over: Partial<StatusDto> = {}): StatusDto {
  return {
    state: "running",
    nodeId: "aa11",
    meshId: "mm00",
    name: "Desk",
    role: "host",
    tags: [],
    membersTotal: 1,
    membersOnline: 1,
    services: 0,
    gatewayUrl: null,
    ports: [],
    daemonVersion: "0.1.0",
    dataDir: "C:/data",
    ...over,
  };
}

/** A minimal QR as the Rust side renders it: one path, a viewBox, nothing else. */
export const QR_SVG =
  '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 29 29" shape-rendering="crispEdges"><path d="M4 4h7v1h-7zM12 4h1v1h-1z" fill="currentColor"/></svg>';

export function service(over: Partial<ServiceDto> & { name: string }): ServiceDto {
  return {
    kind: "static",
    version: 1,
    placement: { kind: "allHosts" },
    placedHosts: [],
    local: { kind: "ready", version: 1 },
    limits: { memoryMb: 64, requestTimeoutMs: 10_000, maxBodyBytes: 8 * 1024 * 1024 },
    outbound: [],
    dataBytes: 0,
    ...over,
  };
}
