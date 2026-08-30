# Origin and attribution

This repository started as an extraction from
[AndrewAltimit/template-repo](https://github.com/AndrewAltimit/template-repo), released into
the public domain under the [Unlicense](LICENSE) with the [MIT License](LICENSE-MIT) as a
fallback. Both licences permit this use; the MIT copyright notice is kept in `LICENSE-MIT`.

## What was taken

| Path here | Taken from | State |
|---|---|---|
| `crates/mcp-gaea2` | `tools/mcp/mcp_gaea2` | heavily reworked, see below |
| `crates/mcp-core` | `tools/mcp/mcp_core_rust/crates/mcp-core` | vendored, unchanged except lint fixes |
| `crates/mcp-client` | `tools/mcp/mcp_core_rust/crates/mcp-client` | vendored, unchanged except lint fixes |
| `crates/mcp-macros` | `tools/mcp/mcp_core_rust/crates/mcp-macros` | vendored, unchanged |

Nothing else from that repository is included. The upstream project states that it does not
accept external contributions, so the changes below live here rather than as a pull request.

## What changed in the server

The upstream server described Gaea 2.2.6.0 from a hand-written table that had drifted from the
product: it accepted 23 node types that do not exist and rejected 15 that do, invented port
layouts, dropped save definitions, and could not launch a build at all on Gaea 2.3. This fork
derives its schema from the installed Gaea instead. See the repository README for the list.
