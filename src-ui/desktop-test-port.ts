import { randomInt } from "node:crypto";
import { createServer, type Server } from "node:net";

const PORT_ATTEMPTS = 16;

function* candidatePorts(): Generator<number> {
  for (let attempt = 0; attempt < PORT_ATTEMPTS; attempt++) yield randomInt(49152, 65536);
}

function isOccupied(error: unknown): boolean {
  return error instanceof Error && "code" in error && (error.code === "EADDRINUSE" || error.code === "EACCES");
}

async function bindPort(server: Server, port: number): Promise<void> {
  if (!Number.isInteger(port) || port < 49152 || port > 65535) {
    throw new RangeError("桌面验收端口必须在 49152-65535 范围内");
  }
  await new Promise<void>((resolve, reject) => {
    const cleanup = () => {
      server.off("error", onError);
      server.off("listening", onListening);
    };
    const onError = (error: Error) => { cleanup(); reject(error); };
    const onListening = () => { cleanup(); resolve(); };
    server.once("error", onError);
    server.once("listening", onListening);
    try { server.listen(port, "127.0.0.1"); } catch (error) { cleanup(); reject(error); }
  });
}

/** 保持实际监听，避开浏览器禁用端口；占用或系统保留端口仅作有界换口重试。 */
export async function listenOnBrowserSafePort(server: Server, ports: Iterable<number> = candidatePorts()): Promise<number> {
  let attempts = 0;
  for (const port of ports) {
    if (attempts++ >= PORT_ATTEMPTS) break;
    try {
      await bindPort(server, port);
      return port;
    } catch (error) {
      if (!isOccupied(error)) throw error;
    }
  }
  throw new Error("没有可绑定的浏览器安全端口（最多尝试 16 次）");
}

/** 先实际探测绑定再交给外部进程；启动者负责失败清理，并以 EADDRINUSE/EACCES 报告抢占。 */
export async function startOnBrowserSafePort<T>(start: (port: number) => Promise<T>, ports: Iterable<number> = candidatePorts()): Promise<T> {
  let attempts = 0;
  for (const port of ports) {
    if (attempts++ >= PORT_ATTEMPTS) break;
    const reservation = createServer();
    try {
      await bindPort(reservation, port);
      await new Promise<void>((resolve, reject) => reservation.close((error) => error ? reject(error) : resolve()));
      return await start(port);
    } catch (error) {
      if (!isOccupied(error)) throw error;
    } finally {
      if (reservation.listening) await new Promise<void>((resolve) => reservation.close(() => resolve()));
    }
  }
  throw new Error("外部进程无法使用浏览器安全端口（最多尝试 16 次）");
}
