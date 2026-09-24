import { describe, expect, it } from "vitest";
import { keyTone, noAutofill, windows } from "./kit.svelte";
import type { KeyView } from "../../lib/types";

/// `keyTone` is the one place a key's *tier* becomes a colour, and the tiers are
/// the CLI's own strings — `src/view.rs` emits them and nothing here parses
/// them. So the mapping is tested against the exact strings the CLI prints,
/// including the `inline (plaintext)` form the lint exists for.
describe("keyTone", () => {
  const key = (tier: string, missing = false): KeyView => ({ tier, missing });

  it("treats a missing key as the one actually-broken state", () => {
    expect(keyTone(key("missing", true))).toBe("bad");
    // `missing` wins even if the tier string says something else, because the
    // flag is what the CLI computed and the tier is only its label.
    expect(keyTone(key("env OPENCODE_API_KEY", true))).toBe("bad");
  });

  it("tones the three resolved homes ok", () => {
    expect(keyTone(key("env OPENCODE_API_KEY"))).toBe("ok");
    expect(keyTone(key("store"))).toBe("ok");
  });

  it("lints an inline key as a warning rather than a failure", () => {
    // The exact prefix matters: `view.rs` writes `inline (plaintext)`.
    expect(keyTone(key("inline (plaintext)"))).toBe("warn");
    expect(keyTone(key("inline"))).toBe("warn");
  });

  it("gives `not required` no tone at all", () => {
    // A keyless provider (searxng) is not a warning; a tone here would put a
    // yellow badge on a correct config.
    expect(keyTone(key("not required"))).toBe("");
  });
});

describe("windows", () => {
  it("renders a null context window as an em dash", () => {
    expect(windows(null)).toBe("—");
  });

  it("groups thousands the way the window does", () => {
    expect(windows(200000)).toBe("200,000");
    expect(windows(0)).toBe("0");
  });
});

describe("noAutofill", () => {
  /// The shape, pinned, because the spread is the only thing the panels do with
  /// it and a dropped key would leave nine of ten fields opted out — silently,
  /// since nothing else reads this object. `spellcheck` is `false` and not
  /// `"false"`: it is the one of the three the platform takes as a boolean.
  it("carries all three opt-outs", () => {
    expect(noAutofill).toEqual({
      autocomplete: "off",
      autocorrect: "off",
      spellcheck: false,
    });
  });
});
