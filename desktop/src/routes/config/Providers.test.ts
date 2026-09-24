import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/svelte";
import { userEvent } from "@testing-library/user-event";
import Providers from "./Providers.svelte";
import type { ProviderView } from "../../lib/types";

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
///
/// It also pins that the address editor sends **one** field and no other. The
/// prop is named `onSetBaseUrl` rather than a generic `onSetScalar` precisely so
/// that stays true: `set-provider` is a generic scalar setter on the CLI side and
/// writes `spec` with no validation, so a generic prop here would put an
/// unvalidated write in the window's reach.

const provider = (over: Partial<ProviderView> = {}): ProviderView => ({
  id: "zen",
  spec: "anthropic",
  base_url: "https://opencode.ai/zen",
  api_key_env: null,
  extra_header_names: [],
  key: { tier: "env OPENCODE_API_KEY", missing: false },
  ...over,
});

function mount(
  over: {
    providers?: ProviderView[];
    stagedKeys?: string[];
    busy?: boolean;
    changed?: boolean;
    error?: string | null;
  } = {},
  handlers: Partial<Record<string, unknown>> = {},
) {
  return render(Providers, {
    props: {
      providers: [],
      stagedKeys: [],
      busy: false,
      changed: false,
      error: null,
      onAdd: async () => {},
      onRemove: async () => {},
      onSetBaseUrl: async () => {},
      onKeyEnv: async () => {},
      onStageKey: async () => {},
      onUnstageKey: async () => {},
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
  await userEvent.click(
    screen.getAllByRole("button", { name: "Change key" })[0],
  );
  const input = screen.getByLabelText(/variable name|value$/);
  expect(input.id).toBe(`kval-${id}`);
  await userEvent.type(input, value);
}

describe("Providers — the three key homes", () => {
  it("sends the env-var name as a name, not a value", async () => {
    const seen: Array<[string, string]> = [];
    mount(
      { providers: [provider()] },
      {
        onKeyEnv: async (id: string, envVar: string) => {
          seen.push([id, envVar]);
        },
      },
    );

    await openEditor("zen", "ZEN_KEY");
    await userEvent.click(screen.getByRole("button", { name: "Point at it" }));

    // A variable *name* goes to `onKeyEnv` — nothing here stages a slot, because
    // pointing at an env var writes `api_key_env` into the document instead.
    expect(seen).toEqual([["zen", "ZEN_KEY"]]);
    expect(
      screen.queryByRole("button", { name: "Unstage key" }),
    ).not.toBeInTheDocument();
  });

  it("files a pasted value under the `provider.<id>` slot", async () => {
    const seen: Array<[string, string]> = [];
    mount(
      { providers: [provider()] },
      {
        onStageKey: async (slot: string, value: string) => {
          seen.push([slot, value]);
        },
      },
    );

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
    expect(
      (screen.getByLabelText("variable name") as HTMLInputElement).type,
    ).toBe("text");

    await userEvent.selectOptions(screen.getByLabelText("key home"), "paste");
    // Paste mode: it is a secret, so it is masked — the tier never renders a
    // value back, and neither does the field the value is typed into.
    expect((screen.getByLabelText("value") as HTMLInputElement).type).toBe(
      "password",
    );
  });

  it("offers the unstage control only once its slot is staged", async () => {
    // Unstaged: no control, because there is nothing to give up.
    const { unmount } = mount({ providers: [provider()] });
    expect(
      screen.queryByRole("button", { name: "Unstage key" }),
    ).not.toBeInTheDocument();
    unmount();

    // Staged: the control appears, keyed on the *slot*. The provider row carries
    // no `staged` badge of its own — the tier badge already says `store` once the
    // value is in — so the button is the whole signal here.
    mount({ providers: [provider()], stagedKeys: ["provider.zen"] });
    expect(
      screen.getByRole("button", { name: "Unstage key" }),
    ).toBeInTheDocument();
  });

  it("reads a staged slot that belongs to another provider as not staged", async () => {
    // The lookup is per-provider, so a slot for a *different* id must not light
    // this provider up — that would offer to unstage a value it never staged.
    mount({ providers: [provider()], stagedKeys: ["provider.zen-go"] });
    expect(
      screen.queryByRole("button", { name: "Unstage key" }),
    ).not.toBeInTheDocument();
  });

  it("sends the slot back when unstaging, not the id", async () => {
    const seen: string[] = [];
    mount(
      { providers: [provider()], stagedKeys: ["provider.zen"] },
      {
        onUnstageKey: async (slot: string) => {
          seen.push(slot);
        },
      },
    );

    await userEvent.click(screen.getByRole("button", { name: "Unstage key" }));
    expect(seen).toEqual(["provider.zen"]);
  });

  it("will not submit an empty value", async () => {
    const seen: string[] = [];
    mount(
      { providers: [provider()] },
      {
        onStageKey: async (slot: string) => {
          seen.push(slot);
        },
      },
    );

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

/// The `unsaved` marker is the one thing in this panel that is driven by the
/// parent's state rather than the panel's own, so both directions get pinned:
/// the flag renders a marker on the heading, and its absence renders nothing.
/// Read off the `h2` rather than a bare `getByText("unsaved")`, which would
/// match any badge in the panel as well as the heading.
describe("Providers — the unsaved marker", () => {
  it("marks the heading when the parent says so", () => {
    mount({ providers: [provider()], changed: true });
    expect(
      screen.getByRole("heading", { name: /Providers unsaved/ }),
    ).toBeInTheDocument();
  });

  it("marks nothing when nothing is unsaved", () => {
    mount({ providers: [provider()] });
    expect(
      screen.queryByRole("heading", { name: /unsaved/ }),
    ).not.toBeInTheDocument();
  });
});

describe("Providers — the panel's edges", () => {
  it("says a route cannot point anywhere without a provider", () => {
    mount({ providers: [] });
    expect(
      screen.getByText(/A route cannot point anywhere/),
    ).toBeInTheDocument();
  });

  it("renders a refusal in the CLI's own words", () => {
    mount({
      providers: [provider()],
      error: "removing zen would strand a route",
    });
    expect(
      screen.getByText("removing zen would strand a route"),
    ).toBeInTheDocument();
  });

  it("names extra headers without ever showing their values", () => {
    // The redaction boundary reaches into this panel: `view.rs` emits header
    // *names* only, and the copy has to say so or a reader assumes the values
    // were omitted from the panel rather than from the wire.
    mount({
      providers: [
        provider({ extra_header_names: ["x-opencode-session", "x-trace"] }),
      ],
    });
    expect(
      screen.getByText(
        /headers: x-opencode-session, x-trace \(values hidden\)/,
      ),
    ).toBeInTheDocument();
  });

  it("reports the header line only when there are headers", () => {
    mount({ providers: [provider({ extra_header_names: [] })] });
    expect(screen.queryByText(/\(values hidden\)/)).not.toBeInTheDocument();
  });

  it("will not add a provider with no id or no base_url", async () => {
    const seen: unknown[] = [];
    mount(
      { providers: [] },
      {
        onAdd: async (...args: unknown[]) => {
          seen.push(args);
        },
      },
    );

    await userEvent.click(screen.getByRole("button", { name: "Add provider" }));
    const add = screen.getByRole("button", { name: "Add provider" });
    // A provider with no id has no slot to stage a key under and no route can
    // name it, so the form refuses locally rather than sending a bad add.
    expect(add).toBeDisabled();

    await userEvent.type(screen.getByLabelText("id"), "zen-go");
    expect(add).toBeDisabled();
    // The label reads "where requests go" rather than `base_url`: the per-row
    // address editor already carries that accessible name, and two labels sharing
    // one text make `getByLabelText` ambiguous for the tests and for a screen
    // reader scanning two fields it cannot tell apart. The hint keeps the
    // document's spelling.
    await userEvent.type(
      screen.getByLabelText("where requests go"),
      "https://opencode.ai/zen/go",
    );
    expect(add).toBeEnabled();

    await userEvent.click(add);
    expect(seen).toEqual([
      ["zen-go", "anthropic", "https://opencode.ai/zen/go"],
    ]);
  });

  it("closes the add form after a successful add", async () => {
    mount({ providers: [] });
    await userEvent.click(screen.getByRole("button", { name: "Add provider" }));
    await userEvent.type(screen.getByLabelText("id"), "zen-go");
    await userEvent.type(
      screen.getByLabelText("where requests go"),
      "https://x.test",
    );
    await userEvent.click(screen.getByRole("button", { name: "Add provider" }));

    // The draft is cleared and the form closed, so a second click cannot send
    // the same provider twice.
    await screen.findByRole("button", { name: "Add provider" });
    expect(screen.queryByLabelText("id")).not.toBeInTheDocument();
  });

  it("disables every mutating control while busy", () => {
    mount({ providers: [provider()], busy: true });
    for (const name of [
      "Edit address",
      "Change key",
      "Remove",
      "Add provider",
    ]) {
      expect(screen.getByRole("button", { name })).toBeDisabled();
    }
  });
});

/// The address editor is the one control here that came from the plan's
/// out-of-scope list — `base_url` was unwritable from the window while the CLI
/// had always accepted it. Two things it has to get right, and both are pinned:
///
/// 1. It sends **only** `base_url`, through a per-field prop. Pinned because the
///    natural-looking refactor (a generic `onSetScalar`, matching `Routes`) would
///    widen the window to every provider scalar — `spec` included, which the
///    generic op writes without validating.
/// 2. It refuses an empty draft **locally**. `base_url` is a required,
///    non-`Option` field and the op's value is a plain `String`, so there is no
///    shape of this op that clears the key: `""` would write an empty string and
///    leave a provider nothing can reach, which `config::validate` does not catch.
describe("Providers — the address editor", () => {
  it("sends the address and nothing else", async () => {
    const seen: Array<[string, string]> = [];
    mount(
      { providers: [provider()] },
      {
        onSetBaseUrl: async (id: string, value: string) => {
          seen.push([id, value]);
        },
      },
    );

    await userEvent.click(screen.getByRole("button", { name: "Edit address" }));
    const input = screen.getByLabelText("base_url");
    // Seeded from the row's own view — the field is one the redaction boundary
    // passes through unchanged, and re-typing the whole URL to change one path
    // segment is the thing this editor exists to avoid.
    expect((input as HTMLInputElement).value).toBe("https://opencode.ai/zen");

    await userEvent.clear(input);
    await userEvent.type(input, "https://opencode.ai/zen/go");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));

    // One field, named for the provider it belongs to. A generic prop would show
    // up here as a key argument, which is the whole point of pinning the shape.
    expect(seen).toEqual([["zen", "https://opencode.ai/zen/go"]]);
  });

  it("trims the draft before sending it", async () => {
    const seen: string[] = [];
    mount(
      { providers: [provider()] },
      {
        onSetBaseUrl: async (_id: string, value: string) => {
          seen.push(value);
        },
      },
    );

    await userEvent.click(screen.getByRole("button", { name: "Edit address" }));
    const input = screen.getByLabelText("base_url");
    await userEvent.clear(input);
    await userEvent.type(input, "  https://x.test  ");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));

    // A trailing space in a URL is invisible in the field and would land in the
    // document verbatim, which `doctor`'s `base-url-shape` then warns about.
    expect(seen).toEqual(["https://x.test"]);
  });

  it("will not save an empty address", async () => {
    const seen: string[] = [];
    mount(
      { providers: [provider()] },
      {
        onSetBaseUrl: async (_id: string, value: string) => {
          seen.push(value);
        },
      },
    );

    await userEvent.click(screen.getByRole("button", { name: "Edit address" }));
    const save = screen.getByRole("button", { name: "Save" });
    await userEvent.clear(screen.getByLabelText("base_url"));

    // `base_url` is required and the op cannot clear it, so an empty submit would
    // write an empty string rather than removing the key. Refused here instead.
    expect(save).toBeDisabled();
    await userEvent.type(screen.getByLabelText("base_url"), "   ");
    expect(save).toBeDisabled();
    expect(seen).toEqual([]);
  });

  it("closes the editor on save and on cancel", async () => {
    mount({ providers: [provider()] });
    const toggle = () => screen.getByRole("button", { name: "Edit address" });

    await userEvent.click(toggle());
    expect(screen.getByLabelText("base_url")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByLabelText("base_url")).not.toBeInTheDocument();

    await userEvent.click(toggle());
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(screen.queryByLabelText("base_url")).not.toBeInTheDocument();
  });

  it("opens the editor only for the row whose button was clicked", async () => {
    // Two providers, one click: a shared `editing` string is what keeps a second
    // row's address from rendering under the first row's heading.
    mount({
      providers: [
        provider(),
        provider({ id: "zen-go", base_url: "https://x.test" }),
      ],
    });
    await userEvent.click(
      screen.getAllByRole("button", { name: "Edit address" })[1],
    );

    const input = screen.getByLabelText("base_url") as HTMLInputElement;
    expect(input.id).toBe("addr-zen-go");
    expect(input.value).toBe("https://x.test");
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
    expectNoAutofill(screen.getByLabelText("where requests go"));
  });

  it("opts the address editor's field out", async () => {
    mount({ providers: [provider()] });
    await userEvent.click(screen.getByRole("button", { name: "Edit address" }));
    // Autofill over a base URL would offer a saved address from somewhere else —
    // the same reason the add form's copy of this field is opted out.
    expectNoAutofill(screen.getByLabelText("base_url"));
  });
});
