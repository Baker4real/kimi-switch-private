import { createServer } from "node:http";
import { describe, expect, it } from "vitest";

import { createServer as createTcpServer, type Server } from "node:net";
import { listenOnBrowserSafePort, startOnBrowserSafePort } from "./desktop-test-port";

async function close(server: Server): Promise<void> {
  if (server.listening) await new Promise<void>((resolve) => server.close(() => resolve()));
}

async function listen(server: Server, port: number): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", resolve);
  });
}

describe("桌面验收的浏览器安全端口", () => {
  it("真实监听高位安全端口并能经 fetch 访问", async () => {
    const server = createServer((_request, response) => response.end("desktop-port-ready"));
    try {
      const port = await listenOnBrowserSafePort(server);
      expect(port).toBeGreaterThanOrEqual(49152);
      expect(port).toBeLessThanOrEqual(65535);
      expect(await (await fetch(`http://127.0.0.1:${port}/`)).text()).toBe("desktop-port-ready");
    } finally {
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    }
  });

  it("交给外部进程前端口被抢占时换到下一个真实可绑定端口", async () => {
    const firstProbe = createTcpServer();
    const secondProbe = createTcpServer();
    const racer = createTcpServer();
    const started: Server[] = [];
    try {
      const first = await listenOnBrowserSafePort(firstProbe);
      const second = await listenOnBrowserSafePort(secondProbe);
      await close(firstProbe);
      await close(secondProbe);
      const attempted: number[] = [];
      const selected = await startOnBrowserSafePort(async (port) => {
        attempted.push(port);
        if (attempted.length === 1) await listen(racer, port);
        const target = createTcpServer();
        started.push(target);
        await listen(target, port);
        return port;
      }, [first, second]);
      expect(attempted).toEqual([first, second]);
      expect(selected).toBe(second);
      expect(racer.listening).toBe(true);
    } finally {
      await Promise.all([firstProbe, secondProbe, racer, ...started].map(close));
    }
  });

  it("候选端口真实被占用时保留原监听者并换口", async () => {
    const occupied = createTcpServer();
    const available = createTcpServer();
    const target = createTcpServer();
    try {
      const busy = await listenOnBrowserSafePort(occupied);
      const free = await listenOnBrowserSafePort(available);
      await close(available);
      expect(await listenOnBrowserSafePort(target, [busy, free])).toBe(free);
      expect(occupied.listening).toBe(true);
    } finally {
      await Promise.all([occupied, available, target].map(close));
    }
  });

  it("连续占用最多探测 16 次，不无限重试", async () => {
    const occupied = createTcpServer();
    const target = createTcpServer();
    try {
      const busy = await listenOnBrowserSafePort(occupied);
      let failures = 0;
      target.on("error", () => failures++);
      await expect(listenOnBrowserSafePort(target, Array(40).fill(busy))).rejects.toThrow("最多尝试 16 次");
      expect(failures).toBe(16);
      expect(target.listening).toBe(false);
      expect(occupied.listening).toBe(true);
    } finally {
      await Promise.all([occupied, target].map(close));
    }
  });

  it("拒绝原故障端口 6697，不向浏览器交付低位端口", async () => {
    const target = createTcpServer();
    await expect(listenOnBrowserSafePort(target, [6697])).rejects.toThrow("49152-65535");
    expect(target.listening).toBe(false);
  });

  it("外部进程的非占用故障直接传播，探测端口已释放", async () => {
    const probe = createTcpServer();
    const reclaimed = createTcpServer();
    try {
      const port = await listenOnBrowserSafePort(probe);
      await close(probe);
      let starts = 0;
      const failure = new Error("启动程序不存在");
      await expect(startOnBrowserSafePort(async () => { starts++; throw failure; }, [port, port])).rejects.toBe(failure);
      expect(starts).toBe(1);
      await listen(reclaimed, port);
      expect(reclaimed.listening).toBe(true);
    } finally {
      await Promise.all([probe, reclaimed].map(close));
    }
  });
});
