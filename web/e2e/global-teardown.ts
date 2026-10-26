import { connect } from "node:net";

const WAIT_MS = 10_000;

function accepts(port: number): Promise<boolean> {
  return new Promise((resolve) => {
    const socket = connect({ host: "127.0.0.1", port });
    socket.once("connect", () => {
      socket.destroy();
      resolve(true);
    });
    socket.once("error", () => {
      resolve(false);
    });
  });
}

/**
 * Asks the test server to stop and waits until its port is closed, so it has dropped its scratch
 * database and exited before Playwright stops it. Windows has no signal to send, and a forced
 * stop would leave the database behind.
 */
export default async function globalTeardown(): Promise<void> {
  const port = Number(process.env.ZIRV_FLOW_BROWSER_TEST_PORT ?? "47700");
  try {
    await fetch(`http://127.0.0.1:${port}/__test/shutdown`, { method: "POST" });
  } catch {
    return; // already gone
  }
  const deadline = Date.now() + WAIT_MS;
  while (Date.now() < deadline) {
    if (!(await accepts(port))) return;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`the test server still listens on port ${port} ${WAIT_MS / 1000} s after the shutdown request`);
}
