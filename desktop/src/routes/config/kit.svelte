<script lang="ts" module>
  /// The shared form vocabulary for the config editor.
  import type { KeyView } from "../../lib/types";

  /// A key is "resolved" or not; the tier says which home answered it.
  ///
  /// The same mapping `Settings.svelte` used read-only, kept here now that the
  /// editor is the only consumer: `missing` is the one state that is actually
  /// broken, `inline` is a lint (a plaintext key in the file), `not required`
  /// has no tone at all, and everything else resolved.
  export function keyTone(k: KeyView): string {
    if (k.missing) return "bad";
    if (k.tier === "not required") return "";
    if (k.tier.startsWith("inline")) return "warn";
    return "ok";
  }

  export const windows = (n: number | null) =>
    n === null ? "—" : n.toLocaleString("en-US");

  /// Opting every text-entry control out of the browser's own help.
  ///
  /// Spread onto each text-entry `<input>` in the editor (`{...noAutofill}`).
  /// Left on, the browser offers a dropdown of the user's saved form history
  /// over a provider id or a base URL, and `autocorrect` would silently rewrite
  /// a value the user pasted. Both are wrong here: these fields hold identifiers
  /// and keys, not prose.
  ///
  /// One definition rather than three attributes repeated on ten inputs, so a
  /// field added later cannot quietly opt back in.
  export const noAutofill = {
    autocomplete: "off",
    autocorrect: "off",
    spellcheck: false,
  } as const;
</script>
