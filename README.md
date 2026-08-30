# gaea2-mcp

An MCP server for [Gaea](https://quadspinner.com/) terrain generation that takes its node
schema from the Gaea build installed on the machine, rather than from a table written by hand.

Everything it knows about nodes — which types exist, their categories, their properties with
declared ranges, the port layout in serialization order, the modifiers Gaea attaches by itself
— is extracted from `Gaea.Nodes.dll` and from the `.terrain` files Gaea ships as examples. A
project it writes opens in the installed build, and a node type it does not recognise is
refused up front instead of producing a file Gaea rejects as corrupt.

Verified against **Gaea 2.3.0.1** on Windows.

## Why the schema is generated

The server this one grew from carried a hand-maintained list describing Gaea 2.2.6.0. By the
time 2.3 shipped, that list accepted 23 node types the product does not have — including
`Output`, which strict validation actually required a graph to contain — and rejected 15 real
ones. Ports were partly invented: `Thermal2` was written with a `Talus` port it does not have,
which fails a build with `Index was outside the bounds of the array`, and export nodes were
stripped of the `Out` port they do have. None of that is visible when the project is created;
it surfaces later, as a corrupt file or an empty build directory.

Regenerating from the installation removes the guesswork, and an upgrade of Gaea becomes a
re-run rather than an edit:

```powershell
pwsh crates/mcp-gaea2/tools/extract_gaea_schema.ps1
```

The script needs `ilspycmd` (`dotnet tool install -g ilspycmd`) and writes
`crates/mcp-gaea2/src/gaea_schema_generated.rs`. Pass `-GaeaPath` if Gaea is not in the
default location.

## Build

```bash
cargo build --release
# binary at target/release/mcp-gaea2
```

## Use with an MCP client

Claude Code, `~/.claude.json` or `.mcp.json`:

```json
{
  "mcpServers": {
    "gaea2": {
      "type": "stdio",
      "command": "C:\\path\\to\\gaea2-mcp\\target\\release\\mcp-gaea2.exe",
      "args": ["--mode", "stdio"],
      "env": {
        "GAEA2_PATH": "C:\\Users\\you\\AppData\\Local\\Programs\\Gaea 2.0\\Gaea.Swarm.exe",
        "GAEA2_OUTPUT_DIR": "C:\\Users\\you\\Documents\\Gaea\\Builds"
      }
    }
  }
}
```

Codex, `~/.codex/config.toml`:

```toml
[mcp_servers.gaea2]
command = 'C:\path\to\gaea2-mcp\target\release\mcp-gaea2.exe'
args = ["--mode", "stdio"]

[mcp_servers.gaea2.env]
GAEA2_PATH = 'C:\Users\you\AppData\Local\Programs\Gaea 2.0\Gaea.Swarm.exe'
GAEA2_OUTPUT_DIR = 'C:\Users\you\Documents\Gaea\Builds'
```

`--mode server` runs the same tools over HTTP instead, on `--port` (default 8000).

## Tools

**Authoring** — `create_gaea2_project`, `create_gaea2_from_template`, `list_gaea2_templates`,
`patch_gaea2_project`, `set_gaea2_save_definition`.

**Inspection** — `gaea2_node_info`, `download_gaea2_project`, `list_gaea2_projects`,
`analyze_workflow_patterns`, `suggest_gaea2_nodes`, `optimize_gaea2_properties`.

**Validation and repair** — `validate_and_fix_workflow`, `repair_gaea2_project`,
`validate_gaea2_runtime`.

**Building** — `run_gaea2_project`, `analyze_gaea2_build`, `analyze_execution_history`.

Three of these are worth calling out:

* `gaea2_node_info` answers "what is this node and what can I set on it" from the installed
  build: category, ports in serialization order, intrinsic modifiers, properties with defaults
  and ranges. Give it a `node_type`, or `search`/`category` to list.
* `analyze_gaea2_build` reads a build directory and reports the first faulting node from
  `CRASH_LOG.txt`. Only the first fault is informative — everything downstream of it reports
  that its input returned no data.
* `patch_gaea2_project` edits node properties in place, checking names against the installed
  build and values against their range, instead of regenerating a graph and losing whatever
  the caller did not restate.

## Notes on Gaea itself

**Build resolution** must be a power of two from 256 to 8192. Gaea does not reject anything
else: it starts the build, works through the first node and exits without writing a file, so
an off-list value looks like a broken graph. Sizes such as 2049 belong to the `Unity` node's
`TargetSize`, not to the build resolution — the server refuses them with that explanation.

**Gaea.Swarm needs a real console.** Capturing its output hands it a pipe, and it dies with
`System.IO.IOException: The handle is invalid` (exit code -532462766) *after* the project has
loaded, which reads like a broken project. The server launches it through
`cmd /c start "Gaea" /wait /min` so the process gets a console of its own, and reads the crash
log back from the build directory.

**Output file names** come from the source node, not from `SaveDefinition.Filename`: a `Unity`
node fed by `Thermal2` writes `Thermal2_Out.raw`. That is Gaea's behaviour, not the server's.

## Licence

[Unlicense](LICENSE), with the [MIT License](LICENSE-MIT) as a fallback for jurisdictions that
do not recognise a public domain dedication. Origin and attribution: [NOTICE.md](NOTICE.md).
