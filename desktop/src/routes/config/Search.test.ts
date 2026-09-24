import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/svelte";
import { userEvent } from "@testing-library/user-event";
import Search from "./Search.svelte";
import type { SearchView } from "../../lib/types";

/// The `[search]` block, which is its own panel now rather than a second heading
/// inside `Providers.svelte`. Two things here are easy to break silently and are
/// what this file exists to pin:
///
/// 1. **The key slot follows the provider.** It is `search.<provider>`, derived
///    here rather than written as a literal, so switching exa → searxng must
///    re-point the slot the badge and the Unstage button look for. A hardcoded
///    `search.exa` would reuse exa's key for searxng.
/// 2. **The three key homes send different commands.** `env` goes through
///    `onSearch({ api_key_env })` — a document write on the `[search]` block,
///    which is what the fixed live bug was: the field did not exist on the CLI's
///    arg struct, so the value was discarded with `rc=0, error: null`. `paste`
///    stages a slot; `clear_inline_key` is the strip-only path.

const search = (over: Partial<SearchView> = {}): SearchView => ({
  provider: "exa",
  base_url: "https://api.exa.ai",
  max_loops: 5,
  key: { tier: "env EXA_API_KEY", missing: false },
  ...over,
});

function mount(
  over: {
    search?: SearchView | null;
    stagedKeys?: string[];
    busy?: boolean;
    changed?: boolean;
    error?: string | null;
  } = {},
  handlers: Partial<Record<string, unknown>> = {},
) {
  return render(Search, {
    props: {
      search: null,
      stagedKeys: [],
      busy: false,
      changed: false,
      error: null,
      onSearch: async () => {},
      onAddSearch: async () => {},
      onRemoveSearch: async () => {},
      onStageKey: async () => {},
      onUnstageKey: async () => {},
      ...over,
      ...handlers,
    },
  });
}

/// Open the key editor and pick one of the two homes.
async function openKey(mode: "env" | "paste") {
  await userEvent.click(screen.getByRole("button", { name: "Change key" }));
  if (mode === "paste") {
    await userEvent.selectOptions(screen.getByLabelText("key home"), "paste");
  }
}

describe("Search — the off state", () => {
  it("offers to add [search] and names the consequence", () => {
    mount({ search: null });
    // The copy names what the state costs rather than the setting: with no
    // `[search]` the server tools are stripped from bridged requests.
    expect(screen.getByText(/server tools are stripped/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add [search]" })).toBeInTheDocument();
  });

  it("sends set-search with no fields at all", async () => {
    const seen: unknown[] = [];
    mount({ search: null }, {
      onAddSearch: async (...args: unknown[]) => {
        seen.push(args);
      },
    });

    await userEvent.click(screen.getByRole("button", { name: "Add [search]" }));

    // `onAddSearch` is a no-arg thunk — the block carries its own `provider` key
    // and the select starts it at the schema default, so there is nothing for
    // the window to pass. The empty array pins that: a later change that started
    // sending `{provider: …}` here would show up as one argument, and the op
    // would then be reached with a field it does not read.
    expect(seen).toEqual([[]]);
  });

  it("hides the add control once [search] exists", () => {
    mount({ search: search() });
    expect(screen.queryByRole("button", { name: "Add [search]" })).not.toBeInTheDocument();
  });
});

describe("Search — the provider select", () => {
  it("sends the provider the user picked", async () => {
    const seen: Array<Record<string, unknown>> = [];
    mount({ search: search() }, {
      onSearch: async (args: Record<string, unknown>) => {
        seen.push(args);
      },
    });

    await userEvent.selectOptions(screen.getByLabelText("provider"), "searxng");
    expect(seen).toEqual([{ provider: "searxng" }]);
  });

  it("offers exactly the two engines the CLI accepts", () => {
    mount({ search: search() });
    const select = screen.getByLabelText("provider") as HTMLSelectElement;
    // `SearchManager::from_config` knows exactly these two, so there is no third
    // value the document could hold that this select could not render — and a
    // provider it does not know makes `view.rs` return `None`, which loads as
    // "Off" and the UI cannot ask for.
    expect([...select.options].map((o) => o.value)).toEqual(["exa", "searxng"]);
  });

  it("points the key slot at the view's provider, not a literal", async () => {
    const seen: string[] = [];
    mount({ search: search({ provider: "searxng" }), stagedKeys: ["search.searxng"] }, {
      onUnstageKey: async (slot: string) => {
        seen.push(slot);
      },
    });

    // The slot is derived from what the *view* reports, which matters after a
    // select: the view comes back from the CLI with the new provider, so the
    // Unstage button looks for the new slot rather than the one on screen when
    // the key was staged.
    await userEvent.click(screen.getByRole("button", { name: "Unstage key" }));
    expect(seen).toEqual(["search.searxng"]);
  });

  it("reads another provider's staged slot as not staged", () => {
    mount({ search: search({ provider: "searxng" }), stagedKeys: ["search.exa"] });
    // exa's key must never satisfy searxng's slot — that is the whole reason the
    // slot is keyed by provider.
    expect(screen.queryByRole("button", { name: "Unstage key" })).not.toBeInTheDocument();
  });

  it("renders a keyless engine as not required, with no tone", () => {
    mount({
      search: search({ provider: "searxng", key: { tier: "not required", missing: false } }),
    });
    expect(screen.getByText("not required")).toBeInTheDocument();
    // `keyTone` maps `not required` to the empty string: a colour would call a
    // working engine's key state a problem.
    expect(screen.getByText("not required").className).not.toMatch(/\b(ok|warn|bad)\b/);
  });
});

describe("Search — the three key homes", () => {
  it("sends the env-var name as `api_key_env` on the search block", async () => {
    const seen: Array<Record<string, unknown>> = [];
    mount({ search: search() }, {
      onSearch: async (args: Record<string, unknown>) => {
        seen.push(args);
      },
    });

    await openKey("env");
    await userEvent.type(screen.getByLabelText("variable name"), "EXA_API_KEY");
    await userEvent.click(screen.getByRole("button", { name: "Point at it" }));

    // The regression this pins, and the reason it asserts the *command's* args
    // rather than a rendered result: `SetSearch` had no `api_key_env` field and
    // no `deny_unknown_fields`, so the CLI answered `rc=0, error: null` and
    // dropped the value. The window marked Search dirty and a later Save
    // reported success with the setting absent from the file — which is exactly
    // the shape of failure a rendering assertion could not see.
    expect(seen).toEqual([{ api_key_env: "EXA_API_KEY" }]);
  });

  it("files a pasted value under the `search.<provider>` slot", async () => {
    const seen: Array<[string, string]> = [];
    mount({ search: search() }, {
      onStageKey: async (slot: string, value: string) => {
        seen.push([slot, value]);
      },
    });

    await openKey("paste");
    await userEvent.type(screen.getByLabelText("value"), "exa-secret");
    await userEvent.click(screen.getByRole("button", { name: "Stage it" }));

    expect(seen).toEqual([["search.exa", "exa-secret"]]);
  });

  it("files a pasted value under the *new* slot after a switch", async () => {
    const seen: Array<[string, string]> = [];
    mount({ search: search({ provider: "searxng" }) }, {
      onStageKey: async (slot: string, value: string) => {
        seen.push([slot, value]);
      },
    });

    await openKey("paste");
    await userEvent.type(screen.getByLabelText("value"), "searx-secret");
    await userEvent.click(screen.getByRole("button", { name: "Stage it" }));

    // The twin of the exa case: the same code path, a different provider, and
    // the slot has to differ — otherwise searxng's key lands on exa's.
    expect(seen).toEqual([["search.searxng", "searx-secret"]]);
  });

  it("switches the input to a password field only in paste mode", async () => {
    mount({ search: search() });
    await openKey("env");
    expect((screen.getByLabelText("variable name") as HTMLInputElement).type).toBe("text");

    await userEvent.selectOptions(screen.getByLabelText("key home"), "paste");
    expect((screen.getByLabelText("value") as HTMLInputElement).type).toBe("password");
  });

  it("sends the explicit strip for the inline key", async () => {
    const seen: Array<Record<string, unknown>> = [];
    mount({ search: search({ key: { tier: "inline (plaintext)", missing: false } }) }, {
      onSearch: async (args: Record<string, unknown>) => {
        seen.push(args);
      },
    });

    const button = screen.getByRole("button", { name: "Clear inline key" });
    expect(button).toBeEnabled();
    await userEvent.click(button);
    expect(seen).toEqual([{ clear_inline_key: true }]);
  });

  it("offers the strip only when there is an inline key", () => {
    // The control is disabled rather than hidden, so it stays the discoverable
    // path to the op: an env-var home has no inline key to clear, and a `store`
    // tier has already had one stripped by the write that put it there.
    for (const tier of ["env EXA_API_KEY", "store", "not required"]) {
      const { unmount } = mount({ search: search({ key: { tier, missing: false } }) });
      expect(screen.getByRole("button", { name: "Clear inline key" })).toBeDisabled();
      unmount();
    }
  });

  it("will not submit an empty value", async () => {
    const seen: string[] = [];
    mount({ search: search() }, {
      onStageKey: async (slot: string) => {
        seen.push(slot);
      },
    });

    await openKey("paste");
    const submit = screen.getByRole("button", { name: "Stage it" });
    expect(submit).toBeDisabled();
    await userEvent.type(screen.getByLabelText("value"), "   ");
    expect(submit).toBeDisabled();
    expect(seen).toEqual([]);
  });
});

describe("Search — the remaining [search] scalars", () => {
  it("sends base_url from the input", async () => {
    const seen: Array<Record<string, unknown>> = [];
    mount({ search: search({ base_url: null }) }, {
      onSearch: async (args: Record<string, unknown>) => {
        seen.push(args);
      },
    });

    const base = screen.getByLabelText("base_url");
    // SearXNG's own default, from `config.example.toml` — the field is a URL the
    // user reads off their own instance, so the placeholder is the worked
    // example rather than an empty box.
    expect(base).toHaveAttribute("placeholder", "http://127.0.0.1:8080");
    await userEvent.type(base, "http://127.0.0.1:8899");
    await userEvent.tab();

    expect(seen).toEqual([{ base_url: "http://127.0.0.1:8899" }]);
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

describe("Search — Remove", () => {
  it("sends remove-search with an empty argument object", async () => {
    const seen: unknown[] = [];
    mount({ search: search() }, {
      onRemoveSearch: async (...args: unknown[]) => {
        seen.push(args);
      },
    });

    await userEvent.click(screen.getByRole("button", { name: "Remove [search]" }));
    // `{}`, never `""`: `parse_args` strips whitespace and refuses an *empty*
    // string with "this op needs arguments", so the op is reached with an empty
    // object or not at all. The op itself takes no args — it reads the provider
    // off the document to decide which slot to stage for deletion.
    expect(seen).toEqual([[]]);
  });

  it("offers Remove only when there is a block to remove", () => {
    const { unmount } = mount({ search: null });
    expect(screen.queryByRole("button", { name: "Remove [search]" })).not.toBeInTheDocument();
    unmount();

    mount({ search: search() });
    expect(screen.getByRole("button", { name: "Remove [search]" })).toBeInTheDocument();
  });
});

describe("Search — the panel's edges", () => {
  it("marks its own heading, off its own flag", () => {
    const { unmount } = mount({ search: search(), changed: true });
    expect(screen.getByRole("heading", { name: /Search unsaved/ })).toBeInTheDocument();
    unmount();

    mount({ search: search() });
    expect(screen.queryByRole("heading", { name: /unsaved/ })).not.toBeInTheDocument();
  });

  it("renders a refusal in the CLI's own words", () => {
    mount({ search: search(), error: "search is already configured" });
    expect(screen.getByText("search is already configured")).toBeInTheDocument();
  });

  it("shows the staged badge keyed on the slot", () => {
    const { unmount } = mount({ search: search(), stagedKeys: ["search.exa"] });
    expect(screen.getByText("staged")).toBeInTheDocument();
    unmount();

    mount({ search: search(), stagedKeys: ["provider.zen"] });
    expect(screen.queryByText("staged")).not.toBeInTheDocument();
  });

  it("disables every mutating control while busy", () => {
    mount({ search: search(), busy: true });
    for (const name of ["provider", "base_url", "max loops"]) {
      expect(screen.getByLabelText(name)).toBeDisabled();
    }
    for (const name of ["Change key", "Remove [search]", "Clear inline key"]) {
      expect(screen.getByRole("button", { name })).toBeDisabled();
    }
  });
});

/// Every text-entry control here has to be opted out of the browser's own help:
/// left on, it offers a dropdown of saved form history over an env-var name, and
/// `autocorrect` rewrites a pasted base URL. Asserted on the rendered element
/// rather than on `noAutofill` itself, because the spread is what the panel
/// actually does; the constant's own shape is pinned in `kit.test.ts`.
function expectNoAutofill(input: HTMLElement) {
  expect(input).toHaveAttribute("autocomplete", "off");
  expect(input).toHaveAttribute("autocorrect", "off");
  expect(input).toHaveAttribute("spellcheck", "false");
}

describe("Search — no field invites the browser's autofill", () => {
  it("opts every text field out", async () => {
    mount({ search: search({ base_url: null }) });
    expectNoAutofill(screen.getByLabelText("base_url"));
    expectNoAutofill(screen.getByLabelText("max loops"));

    await openKey("env");
    expectNoAutofill(screen.getByLabelText("variable name"));
    await userEvent.selectOptions(screen.getByLabelText("key home"), "paste");
    // The password field matters most: autofill over a pasted key would offer to
    // fill it with a saved credential from somewhere else.
    expectNoAutofill(screen.getByLabelText("value"));
  });
});
