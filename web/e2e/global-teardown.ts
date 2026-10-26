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
 * Asks the test server to clean up and stop. The server answers 200 only after it finished its
 * worker and dropped its scratch database, so the run passes or fails on that answer. Windows has
 * no signal to send, and a forced stop would otherwise leave the database behind. The wait for
 * the port is a courtesy so the process has exited before Playwright stops it.
 */
export default async function globalTeardown(): Promise<void> {
  const port = Number(process.env.ZIRV_FLOW_BROWSER_TEST_PORT ?? "47700");
  const reply = await fetch(`http://127.0.0.1:${port}/__test/shutdown`, { method: "POST" });
  if (!reply.ok) {
    throw new Error(`the test server could not clean up (${reply.status}): ${await reply.text()}`);
  }
  const deadline = Date.now() + WAIT_MS;
  while (Date.now() < deadline && (await accepts(port))) {
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
}
