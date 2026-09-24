import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/svelte";
import { userEvent } from "@testing-library/user-event";
import Providers from "./Providers.svelte";
import type { ProviderView, SearchView } from "../../lib/types";

/// This panel is where a key's three homes are chosen, and the wizard's order is
/// the contract: point `api_key_env` at a variable name (which *also* strips an
/// inline key), paste a value into the encrypted store, or leave it unset. Two
/// properties here are easy to break silently and are what this pins:
///
/// 1. The slot a staged value is filed under is `provider.<id>`, and never the
///    provider id or the env var name — the session keys its staged map by slot,
///    so a wrong slot is a value that never applies.
/// 2. The `env` and `paste` buttons send *different* commands. Both are reached
///    through the same input, so getting the branch wrong turns "point at a
///    variable" into "write this variable name into the encrypted store".

const provider = (over: Partial<ProviderView> = {}): ProviderView => ({
  id: "zen",
  spec: "anthropic",
  base_url: "https://opencode.ai/zen",
  api_key_env: null,
  extra_header_names: [],
  key: { tier: "env OPENCODE_API_KEY", missing: false },
  ...over,
});

const search = (over: Partial<SearchView> = {}): SearchView => ({
  provider: "exa",
  base_url: "https://api.exa.ai",
  max_loops: 5,
  key: { tier: "env EXA_API_KEY", missing: false },
  ...over,
});

function mount(
  over: {
    providers?: ProviderView[];
    search?: SearchView | null;
    stagedKeys?: string[];
    busy?: boolean;
    error?: string | null;
  } = {},
  handlers: Partial<Record<string, unknown>> = {},
) {
  return render(Providers, {
    props: {
      providers: [],
      search: null,
      stagedKeys: [],
      busy: false,
      error: null,
      onAdd: async () => {},
      onRemove: async () => {},
      onKeyEnv: async () => {},
      onStageKey: async () => {},
      onUnstageKey: async () => {},
      onSearch: async () => {},
      onAddSearch: async () => {},
      ...over,
      ...handlers,
    },
  });
}

/// Open a provider's key editor and type into it.
///
/// The input's id is derived from the provider id (`kval-zen`) and a `<label
/// for>` points at it, which is what makes `getByLabelText` work at all — an
/// unassociated label leaves a screen reader with a nameless field. Pinning the
/// derivation here keeps that from silently drifting to a shared id.
async function openEditor(id: string, value: string) {
  await userEvent.click(screen.getAllByRole("button", { name: "Change key" })[0]);
  const input = screen.getByLabelText(/variable name|value$/);
  expect(input.id).toBe(`kval-${id}`);
  await userEvent.type(input, value);
}

describe("Providers — the three key homes", () => {
  it("sends the env-var name as a name, not a value", async () => {
    const seen: Array<[string, string]> = [];
    mount({ providers: [provider()] }, {
      onKeyEnv: async (id: string, envVar: string) => {
        seen.push([id, envVar]);
      },
    });

    await openEditor("zen", "ZEN_KEY");
    await userEvent.click(screen.getByRole("button", { name: "Point at it" }));

    // A variable *name* goes to `onKeyEnv` — nothing here stages a slot, because
    // pointing at an env var writes `api_key_env` into the document instead.
    expect(seen).toEqual([["zen", "ZEN_KEY"]]);
    expect(screen.queryByRole("button", { name: "Unstage key" })).not.toBeInTheDocument();
  });

  it("files a pasted value under the `provider.<id>` slot", async () => {
    const seen: Array<[string, string]> = [];
    mount({ providers: [provider()] }, {
      onStageKey: async (slot: string, value: string) => {
        seen.push([slot, value]);
      },
    });

    await userEvent.click(screen.getByRole("button", { name: "Change key" }));
    await userEvent.selectOptions(screen.getByLabelText("key home"), "paste");
    await userEvent.type(screen.getByLabelText("value"), "sk-secret");
    await userEvent.click(screen.getByRole("button", { name: "Stage it" }));

    // The slot is `provider.<id>` — the session's own key, not the id itself.
    // Sending `zen` here would stage a slot the writer never reads.
    expect(seen).toEqual([["provider.zen", "sk-secret"]]);
  });

  it("switches the input to a password field only in paste mode", async () => {
    mount({ providers: [provider()] });
    await userEvent.click(screen.getByRole("button", { name: "Change key" }));

    // Env mode: a variable name is not a secret, so the field is plain text and
    // the label says what it wants.
    expect(screen.getByLabelText("variable name")).toBeInTheDocument();
    expect((screen.getByLabelText("variable name") as HTMLInputElement).type).toBe("text");

    await userEvent.selectOptions(screen.getByLabelText("key home"), "paste");
    // Paste mode: it is a secret, so it is masked — the tier never renders a
    // value back, and neither does the field the value is typed into.
    expect((screen.getByLabelText("value") as HTMLInputElement).type).toBe("password");
  });

  it("offers the unstage control only once its slot is staged", async () => {
    // Unstaged: no control, because there is nothing to give up.
    const { unmount } = mount({ providers: [provider()] });
    expect(screen.queryByRole("button", { name: "Unstage key" })).not.toBeInTheDocument();
    unmount();

    // Staged: the control appears, keyed on the *slot*. The provider row carries
    // no `staged` badge of its own — the tier badge already says `store` once the
    // value is in — so the button is the whole signal here.
    mount({ providers: [provider()], stagedKeys: ["provider.zen"] });
    expect(screen.getByRole("button", { name: "Unstage key" })).toBeInTheDocument();
  });

  it("reads a staged slot that belongs to another provider as not staged", async () => {
    // The lookup is per-provider, so a slot for a *different* id must not light
    // this provider up — that would offer to unstage a value it never staged.
    mount({ providers: [provider()], stagedKeys: ["provider.zen-go"] });
    expect(screen.queryByRole("button", { name: "Unstage key" })).not.toBeInTheDocument();
  });

  it("sends the slot back when unstaging, not the id", async () => {
    const seen: string[] = [];
    mount({ providers: [provider()], stagedKeys: ["provider.zen"] }, {
      onUnstageKey: async (slot: string) => {
        seen.push(slot);
      },
    });

    await userEvent.click(screen.getByRole("button", { name: "Unstage key" }));
    expect(seen).toEqual(["provider.zen"]);
  });

  it("will not submit an empty value", async () => {
    const seen: string[] = [];
    mount({ providers: [provider()] }, {
      onStageKey: async (slot: string) => {
        seen.push(slot);
      },
    });

    await userEvent.click(screen.getByRole("button", { name: "Change key" }));
    await userEvent.selectOptions(screen.getByLabelText("key home"), "paste");

    // The submit button is disabled on an empty draft, and `submitKey` guards a
    // whitespace-only one — an empty paste would otherwise stage a slot that
    // then shadows nothing and silently strips the inline key.
    const submit = screen.getByRole("button", { name: "Stage it" });
    expect(submit).toBeDisabled();
    await userEvent.type(screen.getByLabelText("value"), "   ");
    expect(submit).toBeDisabled();
    expect(seen).toEqual([]);
  });
});

describe("Providers — the panel's edges", () => {
  it("says a route cannot point anywhere without a provider", () => {
    mount({ providers: [] });
    expect(screen.getByText(/A route cannot point anywhere/)).toBeInTheDocument();
  });

  it("renders a refusal in the CLI's own words", () => {
    mount({ providers: [provider()], error: "removing zen would strand a route" });
    expect(screen.getByText("removing zen would strand a route")).toBeInTheDocument();
  });

  it("names extra headers without ever showing their values", () => {
    // The redaction boundary reaches into this panel: `view.rs` emits header
    // *names* only, and the copy has to say so or a reader assumes the values
    // were omitted from the panel rather than from the wire.
    mount({
      providers: [provider({ extra_header_names: ["x-opencode-session", "x-trace"] })],
      search: null,
    });
    expect(
      screen.getByText(/headers: x-opencode-session, x-trace \(values hidden\)/),
    ).toBeInTheDocument();
  });

  it("reports the header line only when there are headers", () => {
    mount({ providers: [provider({ extra_header_names: [] })] });
    expect(screen.queryByText(/\(values hidden\)/)).not.toBeInTheDocument();
  });

  it("will not add a provider with no id or no base_url", async () => {
    const seen: unknown[] = [];
    mount({ providers: [] }, {
      onAdd: async (...args: unknown[]) => {
        seen.push(args);
      },
    });

    await userEvent.click(screen.getByRole("button", { name: "Add provider" }));
    const add = screen.getByRole("button", { name: "Add provider" });
    // A provider with no id has no slot to stage a key under and no route can
    // name it, so the form refuses locally rather than sending a bad add.
    expect(add).toBeDisabled();

    await userEvent.type(screen.getByLabelText("id"), "zen-go");
    expect(add).toBeDisabled();
    await userEvent.type(screen.getByLabelText("base_url"), "https://opencode.ai/zen/go");
    expect(add).toBeEnabled();

    await userEvent.click(add);
    expect(seen).toEqual([["zen-go", "anthropic", "https://opencode.ai/zen/go"]]);
  });

  it("closes the add form after a successful add", async () => {
    mount({ providers: [] });
    await userEvent.click(screen.getByRole("button", { name: "Add provider" }));
    await userEvent.type(screen.getByLabelText("id"), "zen-go");
    await userEvent.type(screen.getByLabelText("base_url"), "https://x.test");
    await userEvent.click(screen.getByRole("button", { name: "Add provider" }));

    // The draft is cleared and the form closed, so a second click cannot send
    // the same provider twice.
    await screen.findByRole("button", { name: "Add provider" });
    expect(screen.queryByLabelText("id")).not.toBeInTheDocument();
  });

  it("disables every mutating control while busy", () => {
    mount({ providers: [provider()], busy: true });
    for (const name of ["Change key", "Remove", "Add provider"]) {
      expect(screen.getByRole("button", { name })).toBeDisabled();
    }
  });
});

/// Every text-entry control in this panel has to be opted out of the browser's
/// own help: left on, it offers a dropdown of saved form history over a provider
/// id or an env-var name, and `autocorrect` rewrites a pasted value. Both are
/// wrong here — these fields hold identifiers and keys, not prose.
///
/// Asserted on the rendered element rather than on `noAutofill` itself, because
/// the spread is what the panel actually does; the constant's own shape is
/// pinned in `kit.test.ts`.
function expectNoAutofill(input: HTMLElement) {
  expect(input).toHaveAttribute("autocomplete", "off");
  expect(input).toHaveAttribute("autocorrect", "off");
  expect(input).toHaveAttribute("spellcheck", "false");
}

describe("Providers — no field invites the browser's autofill", () => {
  it("opts the key field out in both homes", async () => {
    mount({ providers: [provider()] });
    await userEvent.click(screen.getByRole("button", { name: "Change key" }));

    // Env mode is still a text field, and a variable name still gets no help.
    expectNoAutofill(screen.getByLabelText("variable name"));

    await userEvent.selectOptions(screen.getByLabelText("key home"), "paste");
    // The password field matters most: autofill over a pasted key would offer
    // to fill it with a saved credential from somewhere else.
    expectNoAutofill(screen.getByLabelText("value"));
  });

  it("opts the add-provider fields out", async () => {
    mount({ providers: [] });
    await userEvent.click(screen.getByRole("button", { name: "Add provider" }));

    expectNoAutofill(screen.getByLabelText("id"));
    expectNoAutofill(screen.getByLabelText("base_url"));
  });

  it("opts the search fields out", async () => {
    mount({ search: search() });
    expectNoAutofill(screen.getByLabelText("max loops"));

    await userEvent.click(screen.getByRole("button", { name: "Change key" }));
    expectNoAutofill(screen.getByLabelText("variable name"));
    await userEvent.selectOptions(screen.getByLabelText("key home"), "paste");
    expectNoAutofill(screen.getByLabelText("value"));
  });
});

describe("Providers — [search], which shares the key story", () => {
  it("offers to add [search] only when it is off", () => {
    const { unmount } = mount({ search: null });
    expect(screen.getByRole("button", { name: "Add [search] (exa)" })).toBeInTheDocument();
    // The copy names the consequence rather than the setting: with no `[search]`
    // the server tools are stripped from bridged requests.
    expect(screen.getByText(/server tools are stripped/)).toBeInTheDocument();
    unmount();

    mount({ search: search() });
    expect(screen.queryByRole("button", { name: "Add [search] (exa)" })).not.toBeInTheDocument();
  });

  it("files the search key under the literal `search.exa` slot", async () => {
    const seen: Array<[string, string]> = [];
    mount({ search: search() }, {
      onStageKey: async (slot: string, value: string) => {
        seen.push([slot, value]);
      },
    });

    await userEvent.click(screen.getByRole("button", { name: "Change key" }));
    await userEvent.selectOptions(screen.getByLabelText("key home"), "paste");
    await userEvent.type(screen.getByLabelText("value"), "exa-secret");
    await userEvent.click(screen.getByRole("button", { name: "Stage it" }));

    // `search.exa`, spelled out — the slot is a fixed literal here, not derived
    // from the provider id the way `provider.<id>` is.
    expect(seen).toEqual([["search.exa", "exa-secret"]]);
  });

  it("sends an env-var name through onSearch rather than onKeyEnv", async () => {
    const searchArgs: Array<Record<string, unknown>> = [];
    const envArgs: unknown[] = [];
    mount({ search: search() }, {
      onSearch: async (args: Record<string, unknown>) => {
        searchArgs.push(args);
      },
      onKeyEnv: async (...args: unknown[]) => {
        envArgs.push(args);
      },
    });

    await userEvent.click(screen.getByRole("button", { name: "Change key" }));
    await userEvent.type(screen.getByLabelText("variable name"), "EXA_API_KEY");
    await userEvent.click(screen.getByRole("button", { name: "Point at it" }));

    // `[search]` has no provider id, so its env home is `api_key_env` on the
    // search block — routed through `onSearch`, never through `onKeyEnv`.
    expect(searchArgs).toEqual([{ api_key_env: "EXA_API_KEY" }]);
    expect(envArgs).toEqual([]);
  });

  it("sends the search slot back when unstaging", async () => {
    const seen: string[] = [];
    mount({ search: search(), stagedKeys: ["search.exa"] }, {
      onUnstageKey: async (slot: string) => {
        seen.push(slot);
      },
    });

    // The regression this pins: `isStaged` is called with the literal slot here
    // and with `slotFor(id)` in the provider row. Before, it took a provider id
    // and wrapped it in `slotFor` itself, so this call looked for
    // `provider.search.exa` — the Exa badge and this button could never render,
    // and an Exa key that was staged could not be unstaged.
    try {
      await userEvent.click(screen.getByRole("button", { name: "Unstage key" }));
    } finally {
      expect(seen).toEqual(["search.exa"]);
    }
  });

  it("sends max_loops as a number, not the input's string", async () => {
    const seen: Array<Record<string, unknown>> = [];
    mount({ search: search() }, {
      onSearch: async (args: Record<string, unknown>) => {
        seen.push(args);
      },
    });

    // The handler is `onchange`, not `oninput`, so a number typed and left in
    // the field sends nothing until it loses focus — typing alone would make
    // this test pass against a handler that fires per keystroke.
    const loops = screen.getByLabelText("max loops");
    await userEvent.clear(loops);
    await userEvent.type(loops, "3");
    await userEvent.tab();

    // `Number(...)` is load-bearing: a string would serialize into the document
    // as `max_loops = "3"` and the writer would refuse it.
    expect(seen).toEqual([{ max_loops: 3 }]);
  });
});
