import { describe, expect, it } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/svelte";
import { userEvent } from "@testing-library/user-event";
import { callCount, fake, lastCall } from "../test/tauri";
import Settings from "./Settings.svelte";
import type {
  CheckView,
  ConfigView,
  ProviderView,
  RouteView,
  SearchView,
  SessionPayload,
} from "../lib/types";

/// The session lifecycle, which is the whole reason this component exists, plus
/// the sub-tab shell it grew when the one long scroll became five tabs.
///
/// The one rule that matters most is the `dirty` flag: it is set *only* on the
/// path that actually changed something. A refusal arrives as a thrown string
/// carrying the wizard's own message and leaves the session untouched, so a
/// refusal must not light up Save — otherwise the window invites the user to
/// write a document the CLI just declined.
///
/// Nothing here is asserted through the panels' internals; each edit is driven
/// by the same click a user makes, so the shims between panel and `apply` are
/// exercised too. Since a panel is only mounted on its own tab, every edit below
/// is preceded by a real click on that tab — see `openTab`.
///
/// The one thing the split must not cost is attribution. `dirty` alone can only
/// say "somewhere"; a panel's `unsaved` badge sits on its own `<h2>`, which is
/// gone the moment its tab is, so the component also puts a dot on the *tab
/// button*. The tests below pin both, because a marker that only exists on the
/// tab you are already looking at cannot tell you where to look.

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

const provider = (over: Partial<ProviderView> = {}): ProviderView => ({
  id: "zen",
  spec: "anthropic",
  base_url: "https://opencode.ai/zen",
  api_key_env: null,
  extra_header_names: [],
  key: { tier: "env OPENCODE_API_KEY", missing: false },
  ...over,
});

const searchView = (over: Partial<SearchView> = {}): SearchView => ({
  provider: "exa",
  base_url: "https://api.exa.ai",
  max_loops: 5,
  key: { tier: "env EXA_API_KEY", missing: false },
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

/// Switch sub-tabs the way a user does. The panels are mounted per tab, so this
/// is the only way to reach a control on a tab other than the landing one — and
/// reaching it any other way would not exercise the shell that gates it. Matched
/// on a prefix, because a tab button may carry a dot or a Doctor count beside its
/// name and the accessible name picks up the count.
const tab = (name: string) => screen.getByRole("tab", { name: new RegExp(`^${name}`) });
const openTab = (name: string) => userEvent.click(tab(name));

const saveButton = () => screen.getByRole("button", { name: "Save" });
const discardButton = () => screen.getByRole("button", { name: "Discard changes" });

/// The status line is a single `<span>` carrying both sentences
/// (`{status} Nothing is written until you save.`), and its ancestors hold the
/// same prefix, so a substring match would hit several elements. Anchor on the
/// span's full text, which is the one thing only it has.
///
/// `dirtyLine` takes the panel clause verbatim, because the first sentence names
/// the panels holding the edit — `Unsaved changes in Providers.` — and a helper
/// that hard-coded one panel name would pass while the attribution was wrong.
const clean = () =>
  screen.getByText(/^No unsaved changes\. Nothing is written until you save\.$/);
const dirtyLine = (panels: string) =>
  screen.getByText(
    new RegExp(`^Unsaved changes${panels}[^.]*\\. Nothing is written until you save\\.$`),
  );

/// The heading marker, as rendered. Read off the `h2` rather than a bare
/// `getByText("unsaved")`, because a panel's row can carry a `staged` badge in
/// the same style — a heading-scoped query is what keeps the two apart. Only
/// visible while the panel's own tab is showing.
const marked = (panel: string) =>
  screen.getByRole("heading", { name: new RegExp(`${panel} unsaved`) });

/// The dot on a sub-tab button, which is the marker that survives a tab switch.
const tabDot = (name: string) => within(tab(name)).getByTitle("unsaved changes");

describe("Settings — the sub-tab shell", () => {
  it("renders the five tabs with Overview showing first", async () => {
    await mount();

    const names = screen.getAllByRole("tab").map((t) => t.textContent?.trim());
    expect(names).toEqual(["Overview", "Providers", "Search", "Routes", "Doctor"]);
    // Overview is the landing page: the Gateway summary is on screen at first
    // paint, which is what makes the load barrier in `mount` work at all.
    expect(tab("Overview")).toHaveAttribute("aria-selected", "true");
    expect(screen.queryByRole("heading", { name: /unsaved/ })).not.toBeInTheDocument();
  });

  it("summarizes the view on the Overview landing", async () => {
    await mount();
    expect(screen.getByText("127.0.0.1:8710")).toBeInTheDocument();
    // The path, not the word `config` that labels it: the label sits in the same
    // `<div>` as its `<code>`, so `/config/` matches the parent and the child and
    // trips the multiple-elements error. The path's own text is unique.
    expect(screen.getByText("/tmp/config.toml")).toBeInTheDocument();
  });

  it("mounts exactly one panel at a time", async () => {
    await mount(session({ view: { kind: "view", view: configView({ providers: [provider()], routes: [route()] }) } }));

    // The route id is a Routes-panel fact and must not be on Overview.
    expect(screen.queryByText("claude-sonnet-5")).not.toBeInTheDocument();
    await openTab("Routes");
    expect(screen.getByText("claude-sonnet-5")).toBeInTheDocument();
    await openTab("Providers");
    // A regex, not the bare string: `Providers.svelte` renders the whole base URL
    // in one `<span>`, and a string argument to `getByText` is an exact match on
    // the element's *entire* text — so `"opencode.ai/zen"` misses
    // `"https://opencode.ai/zen"`.
    expect(screen.getByText(/opencode\.ai\/zen/)).toBeInTheDocument();
    expect(screen.queryByText("claude-sonnet-5")).not.toBeInTheDocument();
  });

  it("switches selection to the tab that was clicked", async () => {
    await mount();
    await openTab("Search");
    expect(tab("Search")).toHaveAttribute("aria-selected", "true");
    expect(tab("Overview")).toHaveAttribute("aria-selected", "false");
  });

  it("keeps the Save bar reachable from every tab", async () => {
    await mount();
    // Nothing is dirty, so the bar's two buttons are closed — but present, which
    // is the point: the bar is lifted above the tab bodies rather than living on
    // the landing page, so an edit made on Routes is one click from being saved.
    for (const name of ["Overview", "Providers", "Search", "Routes", "Doctor"]) {
      await openTab(name);
      expect(saveButton()).toBeInTheDocument();
      expect(saveButton()).toBeDisabled();
      expect(discardButton()).toBeInTheDocument();
    }
  });

  it("counts Doctor's findings on the Doctor tab", async () => {
    const checks: CheckView[] = [
      { id: "key-resolvable", status: "warn", summary: "…" },
      { id: "config-parse", status: "fail", summary: "…" },
      { id: "listen", status: "ok", summary: "…" },
      { id: "live", status: "skip", summary: "…" },
    ];
    await mount(session(), checks);

    // The same `notable` predicate Doctor's own list uses — `fail` and `warn`,
    // not `ok` and not `skip` — so the count on the button and the rows in the
    // panel cannot disagree.
    expect(within(tab("Doctor")).getByText("2")).toBeInTheDocument();
  });
});

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
    expect(screen.queryAllByTitle("unsaved changes")).toHaveLength(0);
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

    await openTab("Routes");
    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));

    // The op went out against the live session id with the CLI's own arg names.
    expect(lastCall("config_edit_apply")).toMatchObject({
      session: "s1",
      op: "remove-route",
      args: { id: "claude-sonnet-5" },
    });
    await waitFor(() => expect(dirtyLine(" in Routes")).toBeInTheDocument());
    expect(saveButton()).toBeEnabled();
    expect(discardButton()).toBeEnabled();
  });
});

/// The whole point of the attribution: `dirty` alone can only say "somewhere",
/// and a user who scrolled past the edit has no way back to it. Every test here
/// drives a real click, so the panel name travels the shim → `apply` → `mark`
/// path the user's click does.
describe("Settings — which panel is holding the edit", () => {
  it("marks the panel whose op ran, and only that one", async () => {
    await mount();
    fake("config_edit_apply", () => session());

    await openTab("Routes");
    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));

    await waitFor(() => expect(marked("Routes")).toBeInTheDocument());
    expect(dirtyLine(" in Routes")).toBeInTheDocument();
    expect(tabDot("Routes")).toBeInTheDocument();
    expect(within(tab("Providers")).queryByTitle("unsaved changes")).not.toBeInTheDocument();
  });

  it("keeps the edit's dot visible from a tab that is not holding it", async () => {
    await mount();
    fake("config_edit_apply", () => session());

    await openTab("Routes");
    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));
    await waitFor(() => expect(marked("Routes")).toBeInTheDocument());

    await openTab("Overview");
    // The panel's own heading went with its tab, so the dot is the only thing on
    // screen saying where the edit is — which is the reason it exists.
    expect(screen.queryByRole("heading", { name: /unsaved/ })).not.toBeInTheDocument();
    expect(dirtyLine(" in Routes")).toBeInTheDocument();
    expect(tabDot("Routes")).toBeInTheDocument();

    // And it is a way back: the tab it names is the one holding the panel.
    await openTab("Routes");
    expect(marked("Routes")).toBeInTheDocument();
  });

  it("names the panels in screen order, not the order they were edited", async () => {
    // Both panels stocked, and — the part that matters — the *same* session
    // handed back by `apply`. A bare `session()` here would re-seed the view
    // without the provider and the Providers `Remove` below would vanish from
    // under the test.
    const seeded = session({
      view: {
        kind: "view",
        view: configView({ providers: [provider()], routes: [route()] }),
      },
    });
    await mount(seeded);
    fake("config_edit_apply", () => seeded);

    // Routes first, then Providers — the reverse of the order they render in.
    // `Remove` is unambiguous against this fixture: the one route has a single
    // target, and target 0 renders no remove button at all, so the only button
    // by that name is the provider's.
    await openTab("Routes");
    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));
    await waitFor(() => expect(dirtyLine(" in Routes")).toBeInTheDocument());
    await openTab("Providers");
    await userEvent.click(screen.getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(marked("Providers")).toBeInTheDocument());

    // `PANELS.filter(...)`, not an append log: a join over arrival order would
    // read `Routes, Providers` here, which is not the order on screen.
    expect(dirtyLine(" in Providers, Routes")).toBeInTheDocument();
    // Both dots, though only the Providers panel is mounted.
    expect(screen.getAllByTitle("unsaved changes")).toHaveLength(2);
  });

  it("marks Providers for a staged key, which is a plan edit not a document one", async () => {
    await mount(session({ view: { kind: "view", view: configView({ providers: [provider()] }) } }));
    // `stage-key` has its own command, not an `apply` op, so the fake that
    // matters is this one. Leaving `config_edit_apply` unfaked is deliberate:
    // a stray `apply` would reject loudly instead of quietly passing.
    fake("config_edit_stage_key", () => session({ staged_keys: ["provider.zen"] }));

    await openTab("Providers");
    await userEvent.click(screen.getByRole("button", { name: "Change key" }));
    await userEvent.selectOptions(screen.getByLabelText("key home"), "paste");
    await userEvent.type(screen.getByLabelText("value"), "sk-secret");
    await userEvent.click(screen.getByRole("button", { name: "Stage it" }));

    // `stage-key` touches no document key at all, so it never reaches `apply`'s
    // op table — the slot's own name is what says which panel it belongs to.
    await waitFor(() => expect(marked("Providers")).toBeInTheDocument());
    expect(dirtyLine(" in Providers")).toBeInTheDocument();
  });

  it("marks Search for the derived `search.<provider>` slot", async () => {
    await mount(
      session({
        view: { kind: "view", view: configView({ search: searchView() }) },
      }),
    );
    fake("config_edit_stage_key", () => session({ staged_keys: ["search.exa"] }));

    await openTab("Search");
    await userEvent.click(screen.getByRole("button", { name: "Change key" }));
    await userEvent.selectOptions(screen.getByLabelText("key home"), "paste");
    await userEvent.type(screen.getByLabelText("value"), "exa-secret");
    await userEvent.click(screen.getByRole("button", { name: "Stage it" }));

    // The regression this pins: `provider.<id>` and `search.<provider>` are both
    // slots, and a `slot.startsWith("provider")` test would file Search's key
    // under Providers — the one panel that can never resolve it.
    await waitFor(() => expect(marked("Search")).toBeInTheDocument());
    expect(dirtyLine(" in Search")).toBeInTheDocument();
    expect(within(tab("Providers")).queryByTitle("unsaved changes")).not.toBeInTheDocument();
  });

  it("marks nothing for an edit the CLI refused", async () => {
    await mount();
    fake("config_edit_apply", () => {
      throw "a failover strategy needs at least two targets";
    });

    await openTab("Routes");
    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));

    await screen.findByText("a failover strategy needs at least two targets");
    // `mark` is on the success path only, the same as `dirty`: a marker on a
    // panel nothing changed in would be the exact lie this feature exists to fix.
    expect(clean()).toBeInTheDocument();
    expect(screen.queryAllByTitle("unsaved changes")).toHaveLength(0);
  });

  it("clears every marker on save", async () => {
    await mount();
    fake("config_edit_apply", () => session());
    await openTab("Routes");
    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));
    await waitFor(() => expect(marked("Routes")).toBeInTheDocument());

    fake("config_edit_save", () => ({ kind: "saved", session: session() }));
    await userEvent.click(saveButton());

    await waitFor(() => expect(clean()).toBeInTheDocument());
    // The tab dot goes with the heading marker: both read `touchedList`, so a
    // survivor would mean the two had drifted apart.
    expect(screen.queryAllByTitle("unsaved changes")).toHaveLength(0);
    expect(screen.queryByRole("heading", { name: /unsaved/ })).not.toBeInTheDocument();
  });

  it("clears every marker on discard", async () => {
    await mount();
    fake("config_edit_apply", () => session());
    await openTab("Routes");
    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));
    await waitFor(() => expect(marked("Routes")).toBeInTheDocument());

    fake("config_edit_discard", () => undefined);
    await userEvent.click(discardButton());

    // Discard re-seeds the session from the file, so the document the marker
    // described no longer exists — a survivor here would point at an edit that
    // was just thrown away.
    await waitFor(() => expect(callCount("config_edit_load")).toBe(2));
    await waitFor(() => expect(clean()).toBeInTheDocument());
    expect(screen.queryAllByTitle("unsaved changes")).toHaveLength(0);
  });

  it("keeps a session error from being read as a marker", async () => {
    await mount();
    // A session can come back carrying an `error` — `take()` copies it into
    // `problem` — and that is a complaint about the document, not an edit to it.
    // Here the op *succeeds* (the fake resolves), so `dirty` and the Routes
    // marker are both right; what must not happen is the error text retracting
    // the marker or the bar naming a panel that did not hold the edit.
    fake("config_edit_apply", () => session({ error: "the seed is already editable" }));
    await openTab("Routes");
    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));

    await screen.findByText("the seed is already editable");
    await waitFor(() => expect(dirtyLine(" in Routes")).toBeInTheDocument());
    expect(marked("Routes")).toBeInTheDocument();
  });

  it("does not advance the round-robin or re-seed the view on a tab switch", async () => {
    await mount();
    const before = callCount("config_edit_load");

    for (const name of ["Providers", "Search", "Routes", "Doctor", "Overview"]) {
      await openTab(name);
    }

    // Navigation is a local `$state` write and nothing else: switching tabs must
    // not re-read the file, or a user browsing the tabs would silently discard
    // the session's staged keys.
    expect(callCount("config_edit_load")).toBe(before);
    expect(callCount("config_edit_apply")).toBe(0);
  });
});

describe("Settings — an edit the CLI refuses", () => {
  it("keeps the session clean and shows the wizard's own words", async () => {
    await mount();
    // `apply` rethrows the CLI's message verbatim — a string, not an Error.
    fake("config_edit_apply", () => {
      throw "a failover strategy needs at least two targets";
    });

    await openTab("Routes");
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
    await openTab("Routes");
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
    await openTab("Routes");
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
    await openTab("Routes");
    await userEvent.click(screen.getByRole("button", { name: "Remove route" }));
    await waitFor(() => expect(saveButton()).toBeEnabled());

    fake("config_edit_save", () => ({ kind: "refused", message: "config-parse: unparseable" }));
    await userEvent.click(saveButton());

    await screen.findByText("config-parse: unparseable");
    // Still dirty, and still attributed: the edits are still only staged, so the
    // user can fix and retry rather than losing them to a failed save, and the
    // panel that holds them is still the one to look in.
    expect(dirtyLine(" in Routes")).toBeInTheDocument();
    expect(saveButton()).toBeEnabled();
  });
});

describe("Settings — discard", () => {
  it("forgets the staged session and reloads from the file", async () => {
    await mount();
    fake("config_edit_apply", () => session());
    await openTab("Routes");
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
