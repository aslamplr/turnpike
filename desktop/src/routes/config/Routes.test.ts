import { describe, expect, it } from "vitest";
import { render, screen, waitFor } from "@testing-library/svelte";
import { userEvent } from "@testing-library/user-event";
import Routes from "./Routes.svelte";
import type { ProviderView, RouteView } from "../../lib/types";

/// Two rules in this panel come straight from the wizard and are reproduced
/// here rather than re-derived, so each gets pinned:
///
/// 1. A non-`static` strategy is *disabled* below two targets, because the CLI
///    refuses that combination — the window must not be able to ask for a state
///    the writer rejects.
/// 2. **A chain index is the `target` array index, target 0 included.** Every
///    link can be removed (down to one), and every op — move, remove, peak
///    hours — sends `i`, never `i - 1`.

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
  cache_affinity: false,
  respect_peak_hours: false,
  display_name: null,
  context_tokens: null,
  targets: [
    {
      provider: "zen",
      model: "claude-sonnet-4-5",
      display_name: null,
      context_tokens: null,
      peak_hours: [],
      enabled: true,
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
      // Each answers `true` — "the edit landed" — which is what lets the panel
      // close its form; `Routes.svelte` clears its drafts only on that value.
      onAdd: async () => true,
      onRemove: async () => true,
      onSetScalar: async () => true,
      onStrategy: async () => true,
      onAddTarget: async () => true,
      onRemoveTarget: async () => true,
      onMoveTarget: async () => true,
      onPeakHours: async () => true,
      onSetTargetEnabled: async () => true,
      onFetchModels: async () => [],
      onSetAffinity: async () => true,
      onSetPeakAware: async () => true,
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
    expect(options.find((o) => o.value === "peak-hours")?.disabled).toBe(true);
    // The disabled reason is rendered, not just implied.
    expect(options.find((o) => o.value === "failover")?.textContent).toContain(
      "needs 2+ enabled targets",
    );
  });

  it("enables them once a second target exists", () => {
    const two = route({
      targets: [
        { provider: "zen", model: "claude-sonnet-4-5", display_name: null, context_tokens: null, peak_hours: [], enabled: true, spec: null },
        { provider: "zen-go", model: "kimi-k2", display_name: null, context_tokens: null, peak_hours: [], enabled: true, spec: null },
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
  it("marks target 0 as the primary and still offers Remove", () => {
    mount([route()]);
    expect(screen.getByText("zen/claude-sonnet-4-5")).toBeInTheDocument();
    expect(screen.getByText("primary")).toBeInTheDocument();
    // Every chain entry is a real array block now, so target 0 has a Remove
    // button too — disabled only because this route still has one target.
    expect(screen.getByTitle("Remove this target")).toBeDisabled();
  });

  it("sends the chain index for each target's remove button", async () => {
    const seen: Array<[string, number]> = [];
    const three = route({
      targets: [
        { provider: "zen", model: "a", display_name: null, context_tokens: null, peak_hours: [], enabled: true, spec: null },
        { provider: "zen-go", model: "b", display_name: null, context_tokens: null, peak_hours: [], enabled: true, spec: null },
        { provider: "zen", model: "c", display_name: null, context_tokens: null, peak_hours: [], enabled: true, spec: null },
      ],
    });
    mount([three], {
      onRemoveTarget: async (id: string, index: number) => {
        seen.push([id, index]);
      },
    });

    // Every link is an array entry now — all three carry a Remove button.
    const removes = screen.getAllByTitle("Remove this target");
    expect(removes).toHaveLength(3);

    // The chain index *is* the array index. Getting this wrong removes the
    // wrong upstream.
    await userEvent.click(removes[0]);
    expect(seen).toEqual([["claude-sonnet-5", 0]]);

    await userEvent.click(removes[2]);
    expect(seen).toEqual([
      ["claude-sonnet-5", 0],
      ["claude-sonnet-5", 2],
    ]);
  });

  it("moves a target and disables the edge directions", async () => {
    const seen: Array<[string, number, number]> = [];
    const three = route({
      targets: [
        { provider: "zen", model: "a", display_name: null, context_tokens: null, peak_hours: [], enabled: true, spec: null },
        { provider: "zen-go", model: "b", display_name: null, context_tokens: null, peak_hours: [], enabled: true, spec: null },
        { provider: "zen", model: "c", display_name: null, context_tokens: null, peak_hours: [], enabled: true, spec: null },
      ],
    });
    mount([three], {
      onMoveTarget: async (id: string, from: number, to: number) => {
        seen.push([id, from, to]);
        return true;
      },
    });

    const ups = screen.getAllByTitle("Move this target up");
    const downs = screen.getAllByTitle("Move this target down");
    expect(ups).toHaveLength(3);
    // The edges cannot move further out.
    expect(ups[0]).toBeDisabled();
    expect(downs[2]).toBeDisabled();

    await userEvent.click(downs[0]);
    expect(seen).toEqual([["claude-sonnet-5", 0, 1]]);

    await userEvent.click(ups[2]);
    expect(seen).toEqual([
      ["claude-sonnet-5", 0, 1],
      ["claude-sonnet-5", 2, 1],
    ]);
  });

  it("edits a target's peak hours", async () => {
    const seen: Array<[string, number, string[]]> = [];
    const withPeak = route({
      targets: [
        {
          provider: "zen",
          model: "claude-sonnet-4-5",
          display_name: null,
          context_tokens: null,
          peak_hours: ["01:00-04:00"],
          enabled: true,
          spec: null,
        },
      ],
    });
    mount([withPeak], {
      onPeakHours: async (id: string, index: number, windows: string[]) => {
        seen.push([id, index, windows]);
        return true;
      },
    });

    expect(screen.getByText("01:00-04:00")).toBeInTheDocument();
    await userEvent.click(screen.getByText("Peak hours"));

    const input = screen.getByLabelText("peak hours (UTC)");
    expect(input).toHaveValue("01:00-04:00");
    await userEvent.clear(input);
    await userEvent.type(input, "22:00-02:00, 06:00-10:00");
    await userEvent.click(screen.getByText("Save"));

    expect(seen).toEqual([
      ["claude-sonnet-5", 0, ["22:00-02:00", "06:00-10:00"]],
    ]);
  });

  it("closes an open peak editor when a move renumbers the chain", async () => {
    // The editor addresses a position, and a move re-points every position after
    // it — leaving it open would submit the windows to a different target.
    const two = route({
      targets: [
        { provider: "zen", model: "a", display_name: null, context_tokens: null, peak_hours: [], enabled: true, spec: null },
        { provider: "zen-go", model: "b", display_name: null, context_tokens: null, peak_hours: [], enabled: true, spec: null },
      ],
    });
    mount([two], {
      onMoveTarget: async () => true,
      onPeakHours: async () => true,
    });

    await userEvent.click(screen.getAllByText("Peak hours")[0]);
    expect(screen.getByLabelText("peak hours (UTC)")).toBeInTheDocument();

    await userEvent.click(screen.getAllByTitle("Move this target down")[0]);
    await waitFor(() =>
      expect(screen.queryByLabelText("peak hours (UTC)")).not.toBeInTheDocument(),
    );
  });

  it("keeps the peak editor open when the chain edit is refused", async () => {
    // A refusal changed nothing, so the index still addresses the same target
    // and the draft the user typed is still theirs to submit.
    const two = route({
      targets: [
        { provider: "zen", model: "a", display_name: null, context_tokens: null, peak_hours: [], enabled: true, spec: null },
        { provider: "zen-go", model: "b", display_name: null, context_tokens: null, peak_hours: [], enabled: true, spec: null },
      ],
    });
    mount([two], {
      onRemoveTarget: async () => false,
      onPeakHours: async () => true,
    });

    await userEvent.click(screen.getAllByText("Peak hours")[0]);
    await userEvent.click(screen.getAllByTitle("Remove this target")[0]);

    expect(screen.getByLabelText("peak hours (UTC)")).toBeInTheDocument();
  });

  it("checks all seven weekdays by default and sends the checked subset", async () => {
    // The day list is part of the peak-hours config: absent means all seven
    // active, and unchecking a day removes it from what is written.
    const seen: Array<[string, number, string[], string[]]> = [];
    const withPeak = route({
      targets: [
        {
          provider: "zen",
          model: "claude-sonnet-4-5",
          display_name: null,
          context_tokens: null,
          peak_hours: ["01:00-04:00"],
          enabled: true,
          spec: null,
        },
      ],
    });
    mount([withPeak], {
      onPeakHours: async (
        id: string,
        index: number,
        windows: string[],
        days: string[],
      ) => {
        seen.push([id, index, windows, days]);
        return true;
      },
    });

    await userEvent.click(screen.getByText("Peak hours"));
    for (const d of ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]) {
      expect(screen.getByLabelText(`Target 0 ${d}`)).toBeChecked();
    }

    // Drop the weekend; the checked days are what gets written.
    await userEvent.click(screen.getByLabelText("Target 0 Sat"));
    await userEvent.click(screen.getByLabelText("Target 0 Sun"));
    expect(screen.getByLabelText("Target 0 Sat")).not.toBeChecked();

    await userEvent.click(screen.getByText("Save"));
    expect(seen).toEqual([
      ["claude-sonnet-5", 0, ["01:00-04:00"], ["mon", "tue", "wed", "thu", "fri"]],
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

describe("Routes — the affinity and peak flags", () => {
  it("reflects the route's affinity flag on its checkbox", () => {
    mount([route({ strategy: "load-balance", cache_affinity: true })]);
    expect(
      screen.getByLabelText("Cache affinity for claude-sonnet-5"),
    ).toBeChecked();
  });

  it("shows both flags off by default", () => {
    mount([route()]);
    expect(
      screen.getByLabelText("Cache affinity for claude-sonnet-5"),
    ).not.toBeChecked();
    expect(
      screen.getByLabelText("Peak hours for claude-sonnet-5"),
    ).not.toBeChecked();
  });

  it("sends each toggle to its own prop", async () => {
    const seen: Array<[string, string, boolean]> = [];
    mount([route()], {
      onSetAffinity: async (id: string, enabled: boolean) => {
        seen.push([id, "affinity", enabled]);
        return true;
      },
      onSetPeakAware: async (id: string, enabled: boolean) => {
        seen.push([id, "peak", enabled]);
        return true;
      },
    });

    await userEvent.click(
      screen.getByLabelText("Cache affinity for claude-sonnet-5"),
    );
    await userEvent.click(
      screen.getByLabelText("Peak hours for claude-sonnet-5"),
    );
    expect(seen).toEqual([
      ["claude-sonnet-5", "affinity", true],
      ["claude-sonnet-5", "peak", true],
    ]);
  });

  it("puts a flag checkbox back when the write is refused", async () => {
    // Same DOM-before-answer problem as the target toggle: the box flips on
    // click, and Svelte only re-applies `checked={}` when the expression moves.
    mount([route()], { onSetPeakAware: async () => false });
    const box = screen.getByLabelText(
      "Peak hours for claude-sonnet-5",
    ) as HTMLInputElement;

    await userEvent.click(box);
    await waitFor(() => expect(box.checked).toBe(false));
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
        onAdd: async () => true,
        onRemove: async () => true,
        onSetScalar: async () => true,
        onStrategy: async () => true,
        onAddTarget: async () => true,
        onRemoveTarget: async () => true,
        onMoveTarget: async () => true,
        onPeakHours: async () => true,
        onSetTargetEnabled: async () => true,
        onFetchModels: async () => [],
        onSetAffinity: async () => true,
        onSetPeakAware: async () => true,
      },
    });
    expect(screen.getByRole("button", { name: "Add target" })).toBeDisabled();
  });
});

describe("Routes — target enable/disable", () => {
  it("sends the chain index and the state the box now shows", async () => {
    const seen: Array<[string, number, boolean]> = [];
    mount([route()], {
      onSetTargetEnabled: async (id: string, index: number, enabled: boolean) => {
        seen.push([id, index, enabled]);
        return true;
      },
    });

    await userEvent.click(screen.getByLabelText("Target 0 enabled"));
    expect(seen).toEqual([["claude-sonnet-5", 0, false]]);
  });

  it("puts the box back when the write is refused", async () => {
    // The click flips the DOM box before the answer, and Svelte re-applies
    // `checked={}` only when the expression changes. A refusal means the
    // document did not move, so the box must be re-asserted from the answer.
    mount([route()], { onSetTargetEnabled: async () => false });
    const box = screen.getByLabelText("Target 0 enabled") as HTMLInputElement;
    expect(box.checked).toBe(true);

    await userEvent.click(box);
    await waitFor(() => expect(box.checked).toBe(true));
  });

  it("marks the first enabled target as primary, and flags the disabled one", () => {
    const r = route({
      targets: [
        { provider: "zen", model: "a", display_name: null, context_tokens: null, peak_hours: [], enabled: false, spec: null },
        { provider: "zen-go", model: "b", display_name: null, context_tokens: null, peak_hours: [], enabled: true, spec: null },
      ],
    });
    mount([r]);
    expect(screen.getByText("primary")).toBeInTheDocument();
    expect(screen.getByText("disabled")).toBeInTheDocument();
  });

  it("gates a non-static strategy on enabled targets, not chain length", () => {
    // Two blocks, one disabled: nothing to balance across, which is the state
    // the CLI refuses to create.
    const r = route({
      targets: [
        { provider: "zen", model: "a", display_name: null, context_tokens: null, peak_hours: [], enabled: true, spec: null },
        { provider: "zen-go", model: "b", display_name: null, context_tokens: null, peak_hours: [], enabled: false, spec: null },
      ],
    });
    mount([r]);
    const select = screen.getByLabelText("strategy") as HTMLSelectElement;
    const failover = Array.from(select.options).find((o) => o.value === "failover");
    expect(failover?.disabled).toBe(true);
    expect(failover?.textContent).toContain("needs 2+ enabled targets");
  });
});

describe("Routes — the provider model picker", () => {
  it("fills a datalist from the provider and still accepts a custom id", async () => {
    const seen: Array<Record<string, unknown>> = [];
    mount([route()], {
      onFetchModels: async () => [
        { id: "a-model", display_name: "A Model" },
        { id: "b-model", display_name: null },
      ],
      onAddTarget: async (_id: string, args: Record<string, unknown>) => {
        seen.push(args);
        return true;
      },
    });

    const addButtons = () => screen.getAllByRole("button", { name: "Add target" });
    await userEvent.click(addButtons()[0]); // open the form
    await screen.findByText(/2 model\(s\) advertised by zen/);

    const options = Array.from(
      document.querySelectorAll("#tm-models-claude-sonnet-5 option"),
    ).map((o) => (o as HTMLOptionElement).value);
    expect(options).toEqual(["a-model", "b-model"]);

    // The list is a convenience: an id the provider does not advertise is
    // still typed in and submitted as-is.
    await userEvent.type(screen.getByLabelText("model"), "custom-id");
    await userEvent.click(addButtons()[1]); // submit
    expect(seen[0]).toMatchObject({ provider: "zen", model: "custom-id" });
  });

  it("says so when the list cannot be fetched, without blocking the field", async () => {
    mount([route()], {
      onFetchModels: async () => {
        throw new Error("provider unreachable");
      },
    });
    await userEvent.click(screen.getByRole("button", { name: "Add target" }));
    await screen.findByText(/provider unreachable — type the id instead/);
    expect(screen.getByLabelText("model")).toBeEnabled();
  });
});
