# turnpike design documentation

Design documentation for **turnpike**, the local LLM gateway. Start with
[architecture](architecture.md) for the big picture, then dig into whichever
module you care about. The code is the source of truth — if a doc and the code
disagree, the doc is wrong.

| Doc | Covers |
| --- | --- |
| [architecture.md](architecture.md) | System overview, design goals, request lifecycle, module map, relation to Ollama |
| [configuration.md](configuration.md) | Full config reference: `[server]`, `[providers.*]`, `[routes.*]`, `[search]`; resolution rules; defaults |
| [gateway.md](gateway.md) | The proxy: HTTP surface, forwarding pipeline, model remapping, security posture, error shapes |
| [bridge.md](bridge.md) | The Anthropic ↔ OpenAI spec bridge: request, response, and streaming translation |
| [search.md](search.md) | Agentic search middleware: `SearchProvider` trait, Exa, SearXNG, the closed-loop mechanics |
| [launchers.md](launchers.md) | `turnpike launch`: Claude Code env vars, Claude Desktop gateway profile, model resolution |

Each doc follows the same shape: what the component does, how it works (with
the key code paths), and the design decisions behind it — including the
rough edges and the bugs it took to get there.

## Installing turnpike

Prebuilt binaries for macOS (Apple Silicon) and Windows (x86_64) publish on
[GitHub Releases](https://github.com/aslamplr/turnpike/releases) alongside a
`SHA256SUMS` manifest, built and released by `.github/workflows/release.yml`
on each published release. Install with the one-liners in the repo
[README](../README.md#install) — `install.sh` (macOS) or `install.ps1`
(Windows). Other platforms and contributors build from source
(`cargo build --release`).
