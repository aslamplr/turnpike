import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/svelte";
import Doctor from "./Doctor.svelte";
import type { CheckView } from "../../lib/types";

/// Doctor renders what the CLI already decided. The derivations worth pinning
/// are `notable` (which checks earn a table row) and `counts` (the fail/warn
/// tallies), because both quietly drop `ok` and `skip` rows — if that filter
/// breaks, the two findings that matter get buried in a page of passes.

const check = (over: Partial<CheckView>): CheckView => ({
  id: "some-check",
  status: "ok",
  summary: "fine",
  ...over,
});

describe("Doctor", () => {
  it("reports no findings rather than a clean bill of health", () => {
    render(Doctor, { props: { checks: [], path: "/tmp/config.toml", dirty: false } });
    // An empty list is ambiguous and the copy says so — doctor not running and
    // doctor finding nothing look identical. The word `doctor` is wrapped in a
    // `<code>`, so the sentence spans an element boundary and no plain string or
    // regex reaches it; match on the element that holds the whole sentence.
    const empty = document.querySelector(".empty");
    expect(empty?.textContent).toMatch(/either everything checks out/);
    expect(empty?.textContent).toMatch(/could not\s+run/);
    expect(empty?.textContent).toMatch(/Both look like this/);
  });

  it("summarizes an all-clear without a table", () => {
    render(Doctor, {
      props: {
        checks: [check({}), check({ id: "b" }), check({ id: "c", status: "skip" })],
        path: "/tmp/config.toml",
        dirty: false,
      },
    });
    expect(screen.getByText(/all 3 checks passed/)).toBeInTheDocument();
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
  });

  it("lists only the notable checks and tallies fail and warn separately", () => {
    render(Doctor, {
      props: {
        checks: [
          check({ id: "ok-one" }),
          check({ id: "skip-one", status: "skip" }),
          check({ id: "routes-strategy", status: "warn", summary: "one target" }),
          check({ id: "config-parse", status: "fail", summary: "unparseable" }),
        ],
        path: "/tmp/config.toml",
        dirty: false,
      },
    });

    // `ok` and `skip` are filtered out of the table...
    expect(screen.queryByText("ok-one")).not.toBeInTheDocument();
    expect(screen.queryByText("skip-one")).not.toBeInTheDocument();
    // ...and the notable two are listed by id.
    expect(screen.getByText("routes-strategy")).toBeInTheDocument();
    expect(screen.getByText("config-parse")).toBeInTheDocument();

    // Both badges carry their own count; a `fail` is not rolled into `warn`.
    // The digit is a bare text node inside the badge's parent `<span>`, not an
    // element of its own, so there is nothing for an adjacent-sibling selector
    // to match — read each tally off the parent that contains its badge.
    const tally = (tone: string) =>
      Array.from(document.querySelectorAll(".kv > span"))
        .find((s) => s.querySelector(`.badge.${tone}`))
        ?.textContent?.replace(/\s+/g, " ")
        .trim();
    expect(tally("bad")).toBe("fail 1");
    expect(tally("warn")).toBe("warn 1");
  });

  it("renders a check's detail and fix when it has them", () => {
    render(Doctor, {
      props: {
        checks: [
          check({
            id: "key-resolvable",
            status: "warn",
            summary: "no key resolved",
            detail: "provider zen has no reachable key",
            fix: "turnpike setup → Providers",
          }),
        ],
        path: "/tmp/config.toml",
        dirty: false,
      },
    });
    expect(screen.getByText("provider zen has no reachable key")).toBeInTheDocument();
    expect(screen.getByText(/turnpike setup → Providers/)).toBeInTheDocument();
  });

  it("says the findings describe the file on disk until a save", () => {
    const { unmount } = render(Doctor, {
      props: { checks: [check({ status: "warn" })], path: "/tmp/config.toml", dirty: true },
    });
    expect(screen.getByText(/runs against/i)).toBeInTheDocument();
    expect(
      screen.getByText(/staged edits below are not in it yet/),
    ).toBeInTheDocument();
    unmount();

    // Clean: the caveat is absent, because doctor and the session agree.
    render(Doctor, {
      props: { checks: [check({ status: "warn" })], path: "/tmp/config.toml", dirty: false },
    });
    expect(screen.queryByText(/staged edits below/)).not.toBeInTheDocument();
  });

  it("names the file it read", () => {
    render(Doctor, { props: { checks: [], path: "/home/u/.config/turnpike/config.toml", dirty: false } });
    expect(screen.getByText("/home/u/.config/turnpike/config.toml")).toBeInTheDocument();
  });
});
