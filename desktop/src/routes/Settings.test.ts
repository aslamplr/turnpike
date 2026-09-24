import { describe, expect, it } from "vitest";
import { render, screen, waitFor } from "@testing-library/svelte";
import { userEvent } from "@testing-library/user-event";
import { callCount, fake, lastCall } from "../test/tauri";
import Settings from "./Settings.svelte";
import type { CheckView, ConfigView, RouteView, SessionPayload } from "../lib/types";

/// The session lifecycle, which is the whole reason this component exists.
///
/// The one rule that matters most is the `dirty` flag: it is set *only* on the
/// path that actually changed something. A refusal arrives as a thrown string
/// carrying the wizard's own message and leaves the session untouched, so a
/// refusal must not light up Save — otherwise the window invites the user to
/// write a document the CLI just declined.
///
/// Nothing here is asserted through the panels' internals; each edit is driven
/// by the same click a user makes, so the shims between panel and `apply` are
/// exercised too.

const configView = (over: Partial<ConfigView> = {}): ConfigView => ({
  config_path: "/tmp/config.toml",
  listen: "127.0.0.1:8710",
  providers: [],
  routes: [],
  search: null,
  ...over,
});

const route = (over: Partial<RouteView> = {}): RouteView => ({
  id: "claude-sonnet-5",
  strategy: "static",
  display_name: null,
  context_tokens: null,
  targets: [
    { provider: "zen", model: "claude-sonnet-4-5", display_name: null, context_tokens: null, spec: null },
  ],
  ...over,
});

const session = (over: Partial<SessionPayload> = {}): SessionPayload => ({
  id: "s1",
  view: { kind: "view", view: configView({ routes: [route()] }) },
  staged_keys: [],
  ...over,
});

/// The three commands every mount needs. `load` takes a session and doctor its
/// findings; both answer immediately so a test can await the first paint.
function seedLoad(payload = session(), checks: CheckView[] = []) {
  fake("settings_config_path", () => "/tmp/config.toml");
  fake("config_edit_load", () => payload);
  fake("doctor_view", () => checks);
}

/// Render and wait for the first load to land — the component is empty until
/// `config_edit_load` answers, so every query would otherwise race it.
async function mount(payload = session(), checks: CheckView[] = []) {
  seedLoad(payload, checks);
  const result = render(Settings);
  await screen.findByText("Gateway");
  await waitFor(() => expect(lastCall("config_edit_load")).not.toBeNull());
  return result;
}

const saveButton = () => screen.getByRole("button", { name: "Save" });
const discardButton = () => screen.getByRole("button", { name: "Discard changes" });

/// The status line is a single `<span>` carrying both sentences
/// (`{dirty ? … } Nothing is written until you save.`), and its ancestors hold
/// the same prefix, so a substring match would hit several elements. Anchor on
/// the span's full text, which is the one thing only it has.
const clean = () =>
  screen.getByText(/^No unsaved changes\. Nothing is written until you save\.$/);
const dirtyLine = () =>
  screen.getByText(/^Unsaved changes\. Nothing is written until you save\.$/);

describe("Settings — the first load", () => {
  it("seeds a session and leaves everything clean", async () => {
    await mount();

    // The session was seeded and doctor consulted, both with no writes.
    expect(callCount("config_edit_load")).toBe(1);
    expect(callCount("doctor_view")).toBe(1);
    // Nothing is dirty until an edit says so, so both write paths are closed.
    expect(clean()).toBeInTheDocument();
    expect(saveButton()).toBeDisabled();
    expect(discardButton()).toBeDisabled();
  });

  it("renders the redacted view it was handed", async () => {
    await mount();
    // The gateway summary comes from the view, not from a separate probe.
    expect(screen.getByText("127.0.0.1:8710")).toBeInTheDocument();
    expect(screen.getByText("claude-sonnet-5")).toBeInTheDocument();
  });

  it("shows a read failure as an error panel, not a broken window", async () => {
    seedLoad();
    fake("config_edit_load", () => {
      throw new Error("config_edit_load: no binary");
    });
    render(Settings);

    await screen.findByText("Could not read the config");
    expect(screen.getByText(/no binary/)).toBeInTheDocument();
  });

  it("surfaces a session error without discarding the view", async () => {
    // `take()` sets `problem` from `next.error` — a session that came back with
    // a complaint is still a usable session, so the panels must still render.
    await mount(session({ error: "removing zen would strand a route" }));
    expect(screen.getByText("removing zen would strand a route")).toBeInTheDocument();
    expect(screen.getByText("Gateway")).toBeInTheDocument();
  });
});

describe("Settings — an edit that succeeds", () => {
  it("marks the session dirty and opens the write path", async () => {
    await mount();
    fake("config_edit_apply", () => session({ id: "s1" }));

    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));

    // The op went out against the live session id with the CLI's own arg names.
    expect(lastCall("config_edit_apply")).toMatchObject({
      session: "s1",
      op: "remove-route",
      args: { id: "claude-sonnet-5" },
    });
    await waitFor(() => expect(dirtyLine()).toBeInTheDocument());
    expect(saveButton()).toBeEnabled();
    expect(discardButton()).toBeEnabled();
  });
});

describe("Settings — an edit the CLI refuses", () => {
  it("keeps the session clean and shows the wizard's own words", async () => {
    await mount();
    // `apply` rethrows the CLI's message verbatim — a string, not an Error.
    fake("config_edit_apply", () => {
      throw "a failover strategy needs at least two targets";
    });

    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));

    await screen.findByText("a failover strategy needs at least two targets");
    // The point of the whole helper: a refusal changed nothing, so the window
    // must not offer to save it.
    expect(clean()).toBeInTheDocument();
    expect(saveButton()).toBeDisabled();
  });

  it("clears a previous refusal once an edit is accepted", async () => {
    await mount();
    fake("config_edit_apply", () => {
      throw "one target";
    });
    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));
    await screen.findByText("one target");

    // `apply` clears `problem` on entry, so a later success cannot leave a stale
    // refusal sitting above a session that has since moved on.
    fake("config_edit_apply", () => session());
    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));
    await waitFor(() => expect(screen.queryByText("one target")).not.toBeInTheDocument());
  });
});

describe("Settings — save", () => {
  it("adopts the returned session and clears dirty", async () => {
    await mount();
    fake("config_edit_apply", () => session());
    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));
    await waitFor(() => expect(saveButton()).toBeEnabled());

    // `saved` carries the whole session, not just the view: the session stays
    // open across a save so the window keeps its id and its staged slots.
    fake("config_edit_save", () => ({
      kind: "saved",
      session: session({ id: "s1", staged_keys: ["provider.zen"] }),
    }));
    await userEvent.click(saveButton());

    await waitFor(() => expect(clean()).toBeInTheDocument());
    expect(saveButton()).toBeDisabled();
    // Doctor is re-run after a save, because the findings describe the file on
    // disk and the file just changed.
    await waitFor(() => expect(callCount("doctor_view")).toBe(2));
  });

  it("treats a refusal as nothing written", async () => {
    await mount();
    fake("config_edit_apply", () => session());
    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));
    await waitFor(() => expect(saveButton()).toBeEnabled());

    fake("config_edit_save", () => ({ kind: "refused", message: "config-parse: unparseable" }));
    await userEvent.click(saveButton());

    await screen.findByText("config-parse: unparseable");
    // Still dirty: the edits are still only staged, so the user can fix and
    // retry rather than losing them to a failed save.
    expect(dirtyLine()).toBeInTheDocument();
    expect(saveButton()).toBeEnabled();
  });
});

describe("Settings — discard", () => {
  it("forgets the staged session and reloads from the file", async () => {
    await mount();
    fake("config_edit_apply", () => session());
    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));
    await waitFor(() => expect(discardButton()).toBeEnabled());

    fake("config_edit_discard", () => undefined);
    await userEvent.click(discardButton());

    // Discard is a reload: the old id is dropped, then a fresh session is seeded.
    await waitFor(() => expect(lastCall("config_edit_discard")).toEqual({ session: "s1" }));
    await waitFor(() => expect(callCount("config_edit_load")).toBe(2));
    await waitFor(() => expect(clean()).toBeInTheDocument());
  });
});
