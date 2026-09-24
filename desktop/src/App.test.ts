import { describe, expect, it } from "vitest";
import { render, screen, waitFor } from "@testing-library/svelte";
import { userEvent } from "@testing-library/user-event";
import { callCount, emit, fake, lastCall } from "./test/tauri";
import App from "./App.svelte";
import type { CliStatus, UpdateStatus } from "./lib/types";

/// `App` owns two banners whose whole job is to appear in exactly the right
/// state, and both are pure functions of a status the Rust side pushed. So the
/// cases worth pinning are the ones where showing the banner would be *wrong*:
/// a ready CLI, or a dev build with no install source, must render nothing —
/// a banner that offers an install that cannot happen is worse than no banner.
///
/// The child `Settings` panel is part of the tree, so its three commands are
/// faked too. Left unfaked they reject and the panel renders its own error,
/// which would make an assertion about *App's* banners depend on a sibling's
/// failure.

/// Commands every mount needs: four from `App`'s own `onMount`, three from the
/// `Settings` panel that renders inside the default tab.
function seed(cli: CliStatus, update: UpdateStatus | null = null) {
  fake("gateway_status", () => ({ state: "stopped" }));
  fake("autostart_enabled", () => false);
  fake("cli_status", () => cli);
  fake("update_status", () => update);
  fake("settings_config_path", () => "/tmp/config.toml");
  fake("config_edit_load", () => ({
    id: "s1",
    view: {
      kind: "view",
      view: {
        config_path: "/tmp/config.toml",
        listen: "127.0.0.1:8710",
        providers: [],
        routes: [],
        search: null,
      },
    },
    staged_keys: [],
  }));
  fake("doctor_view", () => []);
}

/// Mount and wait for `onMount`'s four reads to settle, so a query does not
/// race the first paint.
async function mount(cli: CliStatus, update: UpdateStatus | null = null) {
  seed(cli, update);
  const result = render(App);
  await waitFor(() => expect(callCount("cli_status")).toBe(1));
  return result;
}

const ready: CliStatus = { state: "ready", path: "/usr/local/bin/turnpike", version: "0.1.8" };

describe("App — the CLI offer", () => {
  it("offers an install when no CLI resolves, and names what is missing", async () => {
    await mount({ state: "missing", payload: "/Applications/turnpike.app/Contents/Resources/bin/turnpike-cli" });
    expect(
      screen.getByText("The turnpike CLI is not installed on this machine."),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Install" })).toBeInTheDocument();
  });

  it("offers a reinstall on a version mismatch, both versions named", async () => {
    await mount({
      state: "versionMismatch",
      path: "/usr/local/bin/turnpike",
      found: "0.1.5",
      expected: "0.1.8",
      payload: null,
    });

    // Naming both versions is the point: the user has to know which one is
    // stale. And a mismatch with no payload still offers — on macOS the install
    // downloads from the release rather than copying the bundle, so gating on
    // `payload` would hide the only way to fix a stale CLI.
    expect(
      screen.getByText("The turnpike CLI here is 0.1.5, but this app needs 0.1.8."),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Reinstall" })).toBeInTheDocument();
  });

  it("offers nothing when a CLI is ready", async () => {
    await mount(ready);
    expect(screen.queryByRole("button", { name: "Install" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Reinstall" })).not.toBeInTheDocument();
    expect(
      screen.queryByText("The turnpike CLI is not installed on this machine."),
    ).not.toBeInTheDocument();
  });

  it("offers nothing when there is no install source at all", async () => {
    // `unavailable` is a dev build with no bundled payload. There is nothing to
    // install *from*, so an Install button would promise a download that cannot
    // start.
    await mount({ state: "unavailable", reason: "no bundled payload" });
    expect(screen.queryByRole("button", { name: "Install" })).not.toBeInTheDocument();
  });

  it("puts the banner away for the rest of the session", async () => {
    await mount({ state: "missing", payload: "/x/turnpike-cli" });
    await userEvent.click(screen.getByRole("button", { name: "Later" }));

    // A dismissal is session-only — it is not a decision never to install, so
    // the next launch offers again. Pinning the disappearance, not persistence.
    await waitFor(() =>
      expect(
        screen.queryByText("The turnpike CLI is not installed on this machine."),
      ).not.toBeInTheDocument(),
    );
  });

  it("installs on demand, shows the advice, and starts the gateway", async () => {
    await mount({ state: "missing", payload: "/x/turnpike-cli" });
    fake("cli_install", () => ({
      status: { state: "ready", path: "/home/u/.local/bin/turnpike", version: "0.1.8" },
      note: "Add /home/u/.local/bin to your PATH.",
    }));
    fake("gateway_start", () => undefined);

    await userEvent.click(screen.getByRole("button", { name: "Install" }));

    // The app adopts the status the install returned rather than re-probing, so
    // the banner clears on the answer it already has.
    await screen.findByText("Add /home/u/.local/bin to your PATH.");
    await waitFor(() =>
      expect(screen.queryByRole("button", { name: "Install" })).not.toBeInTheDocument(),
    );
    // Starting the gateway here covers the nothing-to-run case without touching
    // a live gateway — it is a no-op when one is already running.
    expect(callCount("gateway_start")).toBe(1);
  });

  it("shows an install failure instead of pretending it worked", async () => {
    await mount({ state: "missing", payload: "/x/turnpike-cli" });
    fake("cli_install", () => {
      throw new Error("curl: (22) 404");
    });

    await userEvent.click(screen.getByRole("button", { name: "Install" }));

    await screen.findByText(/curl: \(22\) 404/);
    // The offer stays up: the failure is not an answer, so there is still
    // something to press.
    expect(screen.getByRole("button", { name: "Install" })).toBeInTheDocument();
  });
});

describe("App — the update banner", () => {
  it("says nothing while the check is still running", async () => {
    // `null` is "no check has settled"; `checking` is the live state. Neither
    // is worth a banner on its own, but `checking` does render its line.
    await mount(ready, null);
    expect(screen.queryByText(/Checking for updates/)).not.toBeInTheDocument();
  });

  it("offers the new version with both numbers and the release notes", async () => {
    await mount(ready, {
      state: "available",
      version: "0.2.0",
      current: "0.1.8",
      notes: "Fixes the tray crash.",
    });
    // Both versions, because "an update is available" alone does not tell the
    // user how far behind they are.
    expect(screen.getByText(/0\.2\.0 is available \(you have 0\.1\.8\)/)).toBeInTheDocument();
    expect(screen.getByText(/Fixes the tray crash\./)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Install 0.2.0" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Later" })).toBeInTheDocument();
  });

  it("renders a release with no notes without a dangling separator", async () => {
    await mount(ready, { state: "available", version: "0.2.0", current: "0.1.8", notes: null });
    // The notes are appended with a blank line when present; absent, the
    // sentence must end at the parenthesis rather than trailing off.
    expect(
      screen.getByText(/^turnpike 0\.2\.0 is available \(you have 0\.1\.8\)\.$/),
    ).toBeInTheDocument();
  });

  it("shows the failure in the bad tone with a way to dismiss it", async () => {
    await mount(ready, { state: "failed", reason: "signature mismatch" });
    const line = screen.getByText("signature mismatch");
    expect(line).toBeInTheDocument();
    // The tone is inline, not a class, so this is the only way to pin it.
    expect(line.getAttribute("style")).toContain("var(--bad)");
    expect(screen.getByRole("button", { name: "Dismiss" })).toBeInTheDocument();
  });

  it("offers no buttons mid-download, because neither would be true", async () => {
    await mount(ready, { state: "downloading", version: "0.2.0" });
    // "Later" during a download would lie, and there is nothing to press once
    // the download has started.
    expect(screen.queryByRole("button", { name: /Later|Install/ })).not.toBeInTheDocument();
    expect(screen.getByText(/Downloading turnpike 0\.2\.0/)).toBeInTheDocument();
  });

  it("offers no buttons while installing either", async () => {
    await mount(ready, { state: "installing", version: "0.2.0" });
    expect(screen.queryByRole("button", { name: /Later|Install/ })).not.toBeInTheDocument();
    expect(screen.getByText(/Installing turnpike 0\.2\.0/)).toBeInTheDocument();
  });

  it("installs on demand and leaves the banner up on failure", async () => {
    await mount(ready, { state: "available", version: "0.2.0", current: "0.1.8", notes: null });
    fake("update_install", () => {
      throw new Error("no endpoint");
    });

    await userEvent.click(screen.getByRole("button", { name: "Install 0.2.0" }));

    // The banner is not dismissed on the way in — a failure has to be able to
    // keep the offer visible so Install can be pressed again.
    await waitFor(() => expect(callCount("update_install")).toBe(1));
    expect(screen.getByRole("button", { name: "Install 0.2.0" })).toBeInTheDocument();
  });

  it("follows the event stream, not the click", async () => {
    await mount(ready, null);
    // This is the path a real update takes: Rust pushes `update://status` and
    // the banner reads the store, so an event that arrives out of order still
    // lands — which is why `attach` is exercised rather than the store set by
    // hand.
    emit("update://status", { state: "available", version: "0.3.0", current: "0.1.8", notes: null });

    await screen.findByText(/0\.3\.0 is available/);
  });
});

describe("App — the status bar", () => {
  it("writes the running status with the right plural", async () => {
    await mount(ready);
    emit("gateway://status", { state: "running", listen: "127.0.0.1:8710", routes: 1 });
    await screen.findByText("running on 127.0.0.1:8710 — 1 route");

    emit("gateway://status", { state: "running", listen: "127.0.0.1:8710", routes: 3 });
    await screen.findByText("running on 127.0.0.1:8710 — 3 routes");
  });

  it("gates Start, Stop and Restart on what the gateway is doing", async () => {
    await mount(ready);
    // Stopped: nothing to stop or restart.
    expect(screen.getByRole("button", { name: "Start" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Stop" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Restart" })).toBeDisabled();

    emit("gateway://status", { state: "running", listen: "127.0.0.1:8710", routes: 1 });
    await waitFor(() => expect(screen.getByRole("button", { name: "Start" })).toBeDisabled());
    expect(screen.getByRole("button", { name: "Stop" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Restart" })).toBeEnabled();

    // Crashed: stoppable (it is not running, but the supervisor is retrying),
    // not restartable — a restart is what the supervisor is already doing.
    emit("gateway://status", { state: "crashed", code: 1, restarts: 2, in_ms: 3000 });
    await waitFor(() => expect(screen.getByRole("button", { name: "Restart" })).toBeDisabled());
    expect(screen.getByRole("button", { name: "Stop" })).toBeEnabled();
    expect(screen.getByText(/crashed — restart 2 in 3\.0s/)).toBeInTheDocument();
  });

  it("reports an autostart failure to the same error line as everything else", async () => {
    await mount(ready);
    const box = screen.getByRole("checkbox", { name: "Start at login" }) as HTMLInputElement;
    expect(box.checked).toBe(false);

    // `autostart_set` does not throw: `apply_autostart` catches the platform
    // failure, reports it on `gateway://error`, and returns what actually took
    // effect (`publish_autostart` re-reads the plist, so after a failure that is
    // still `false`). The one writer emits the error for both the window and the
    // tray, which is why the failure has to arrive as an event.
    fake("autostart_set", () => false);
    await userEvent.click(box);

    // The toggle asks for `!$autostart`, computed from the store — with the
    // store at `false`, the click asks for `true`.
    expect(lastCall("autostart_set")).toEqual({ enabled: true });

    emit("gateway://error", "start at login: no plist");
    await screen.findByText("start at login: no plist");

    // The error goes to the shared error panel, which is above the checkbox and
    // is the same line a gateway failure lands on. The checkbox itself is only
    // `checked={$autostart}` — a one-way binding — so it follows the store, and
    // the app's own render, not this test's click, is what moves it.
    emit("gateway://autostart", false);
    await waitFor(() => expect(box.checked).toBe(false));
  });

  it("adopts the state the broadcast reports, not the click", async () => {
    await mount(ready);
    fake("autostart_set", () => true);
    const box = screen.getByRole("checkbox", { name: "Start at login" }) as HTMLInputElement;

    await userEvent.click(box);
    expect(lastCall("autostart_set")).toEqual({ enabled: true });

    // The store only moves on `gateway://autostart` — a real toggle has the Rust
    // side broadcast what `publish_autostart` actually read back off the plist,
    // so the checkbox cannot claim a state the platform refused.
    emit("gateway://autostart", true);
    await waitFor(() => expect(box.checked).toBe(true));
  });
});
