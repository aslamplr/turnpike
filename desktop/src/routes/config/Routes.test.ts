import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/svelte";
import { userEvent } from "@testing-library/user-event";
import Routes from "./Routes.svelte";
import type { ProviderView, RouteView } from "../../lib/types";

/// Two rules in this panel come straight from the wizard and are reproduced
/// here rather than re-derived, so each gets pinned:
///
/// 1. A non-`static` strategy is *disabled* below two targets, because the CLI
///    refuses that combination — the window must not be able to ask for a state
///    the writer rejects.
/// 2. The chain shows target 0 first with no remove button (it is the route's
///    flat `provider`/`model`, not an array entry), and every later link's
///    remove button sends array index `i - 1`, not `i`.

const provider = (id: string): ProviderView => ({
  id,
  spec: "anthropic",
  base_url: `https://example.test/${id}`,
  api_key_env: null,
  extra_header_names: [],
  key: { tier: "env " + id.toUpperCase(), missing: false },
});

const route = (over: Partial<RouteView> = {}): RouteView => ({
  id: "claude-sonnet-5",
  strategy: "static",
  display_name: null,
  context_tokens: null,
  targets: [
    {
      provider: "zen",
      model: "claude-sonnet-4-5",
      display_name: null,
      context_tokens: null,
      spec: null,
    },
  ],
  ...over,
});

function mount(
  routes: RouteView[],
  handlers: Partial<Record<string, unknown>> = {},
  changed = false,
) {
  return render(Routes, {
    props: {
      routes,
      providers: [provider("zen"), provider("zen-go")],
      busy: false,
      changed,
      error: null,
      onAdd: async () => {},
      onRemove: async () => {},
      onSetScalar: async () => {},
      onStrategy: async () => {},
      onAddTarget: async () => {},
      onRemoveTarget: async () => {},
      ...handlers,
    },
  });
}

/// The `unsaved` marker is parent-driven, so both directions get pinned. The
/// panel deliberately claims no more than the heading: a single `Edit details`
/// submit sends two ops (`display_name`, `context_tokens`), so a per-row marker
/// could not be backed by anything this component knows.
describe("Routes — the unsaved marker", () => {
  it("marks the heading only when the parent says the panel is holding an edit", () => {
    const { unmount } = mount([route()], {}, false);
    // A clean panel claims nothing. This is the half that matters: a marker that
    // renders unconditionally would pass a "something is marked" test while
    // telling the user every reload that they have unsaved work.
    expect(screen.queryByText("unsaved")).not.toBeInTheDocument();
    unmount();

    mount([route()], {}, true);
    // Read off the `h2` rather than a bare `getByText("unsaved")`, which is what
    // keeps this assertion about the heading and not about some future row-level
    // badge in the same style.
    expect(screen.getByRole("heading", { name: /Routes unsaved/ })).toBeInTheDocument();
  });
});

describe("Routes — the strategy gate", () => {
  it("offers load-balance and failover only at two or more targets", () => {
    mount([route()]);
    const select = screen.getByLabelText("strategy") as HTMLSelectElement;
    const options = Array.from(select.options);

    expect(options.find((o) => o.value === "static")?.disabled).toBe(false);
    expect(options.find((o) => o.value === "load-balance")?.disabled).toBe(true);
    expect(options.find((o) => o.value === "failover")?.disabled).toBe(true);
    // The disabled reason is rendered, not just implied.
    expect(options.find((o) => o.value === "failover")?.textContent).toContain(
      "needs 2+ targets",
    );
  });

  it("enables them once a second target exists", () => {
    const two = route({
      targets: [
        { provider: "zen", model: "claude-sonnet-4-5", display_name: null, context_tokens: null, spec: null },
        { provider: "zen-go", model: "kimi-k2", display_name: null, context_tokens: null, spec: null },
      ],
    });
    mount([two]);
    const select = screen.getByLabelText("strategy") as HTMLSelectElement;
    const options = Array.from(select.options);

    expect(options.find((o) => o.value === "load-balance")?.disabled).toBe(false);
    expect(options.find((o) => o.value === "failover")?.disabled).toBe(false);
  });

  it("reports the chosen strategy to the parent", async () => {
    const seen: Array<[string, string]> = [];
    mount([route()], {
      onStrategy: async (id: string, strategy: string) => {
        seen.push([id, strategy]);
      },
    });
    await userEvent.selectOptions(screen.getByLabelText("strategy"), "static");
    expect(seen).toEqual([["claude-sonnet-5", "static"]]);
  });
});

describe("Routes — the target chain", () => {
  it("renders target 0 as the route's own provider/model with no remove button", () => {
    mount([route()]);
    expect(screen.getByText("zen/claude-sonnet-4-5")).toBeInTheDocument();
    expect(screen.getByText("the route's own provider/model")).toBeInTheDocument();
    // Nothing to remove behind target 0, so there is no Remove control.
    expect(screen.queryByTitle("Remove this target")).not.toBeInTheDocument();
  });

  it("sends array index i-1 for the target at chain position i", async () => {
    const seen: Array<[string, number]> = [];
    const three = route({
      targets: [
        { provider: "zen", model: "a", display_name: null, context_tokens: null, spec: null },
        { provider: "zen-go", model: "b", display_name: null, context_tokens: null, spec: null },
        { provider: "zen", model: "c", display_name: null, context_tokens: null, spec: null },
      ],
    });
    mount([three], {
      onRemoveTarget: async (id: string, index: number) => {
        seen.push([id, index]);
      },
    });

    // Three links: target 0 has no button, positions 1 and 2 do.
    const removes = screen.getAllByTitle("Remove this target");
    expect(removes).toHaveLength(2);

    // The *first* remove button is chain position 1 → array index 0. Getting
    // this wrong shifts every removal and takes out the wrong upstream.
    await userEvent.click(removes[0]);
    expect(seen).toEqual([["claude-sonnet-5", 0]]);

    await userEvent.click(removes[1]);
    expect(seen).toEqual([
      ["claude-sonnet-5", 0],
      ["claude-sonnet-5", 1],
    ]);
  });

  it("renders exactly the targets it is handed, synthesizing nothing", () => {
    // The Rust view is what guarantees target 0 exists — `targets()` synthesizes
    // it there. This component does not: it iterates the array it is given, so an
    // empty array is an empty chain rather than a phantom link. Pinning that is
    // what makes a view-side regression (target 0 dropped from the payload) show
    // up as an empty chain instead of passing on a link the client invented.
    const { container } = mount([route({ targets: [] })]);
    expect(screen.queryByText("zen/claude-sonnet-4-5")).not.toBeInTheDocument();
    expect(container.querySelectorAll(".chain .link")).toHaveLength(0);
  });
});

/// Every text-entry control here is opted out of the browser's own help, for the
/// same reason as `Providers` — these fields hold route ids, upstream model ids
/// and a number, and autofill over any of them offers the wrong value.
///
/// Asserted on the rendered element because the spread is what the panel does;
/// the constant's own shape is pinned in `kit.test.ts`.
function expectNoAutofill(input: HTMLElement) {
  expect(input).toHaveAttribute("autocomplete", "off");
  expect(input).toHaveAttribute("autocorrect", "off");
  expect(input).toHaveAttribute("spellcheck", "false");
}

describe("Routes — no field invites the browser's autofill", () => {
  it("opts the route's own detail fields out", async () => {
    mount([route()]);
    await userEvent.click(screen.getByRole("button", { name: "Edit details" }));

    expectNoAutofill(screen.getByLabelText("display_name"));
    expectNoAutofill(screen.getByLabelText("context_tokens"));
  });

  it("opts the add-target field out", async () => {
    mount([route()]);
    await userEvent.click(screen.getByRole("button", { name: "Add target" }));

    expectNoAutofill(screen.getByLabelText("model"));
  });

  it("opts the add-route fields out", async () => {
    mount([route()]);
    await userEvent.click(screen.getByRole("button", { name: "Add route" }));

    expectNoAutofill(screen.getByLabelText("id (the client-facing model)"));
    expectNoAutofill(screen.getByLabelText("model (upstream)"));
  });
});

describe("Routes — empty and refusal states", () => {
  it("says nothing can resolve a model without a route", () => {
    mount([]);
    expect(screen.getByText(/Nothing can resolve a model/)).toBeInTheDocument();
  });

  it("renders a refusal in the CLI's own words", () => {
    mount([route()], {
      error: "a failover strategy needs at least two targets",
    });
    expect(
      screen.getByText("a failover strategy needs at least two targets"),
    ).toBeInTheDocument();
  });

  it("will not open the add-target form with no providers to point at", () => {
    render(Routes, {
      props: {
        routes: [route()],
        providers: [],
        busy: false,
        changed: false,
        error: null,
        onAdd: async () => {},
        onRemove: async () => {},
        onSetScalar: async () => {},
        onStrategy: async () => {},
        onAddTarget: async () => {},
        onRemoveTarget: async () => {},
      },
    });
    expect(screen.getByRole("button", { name: "Add target" })).toBeDisabled();
  });
});
