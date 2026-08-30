# gaea2-mcp

An MCP server for [Gaea](https://quadspinner.com/) terrain generation that takes its node
schema from the Gaea build installed on the machine, rather than from a table written by hand.

Everything it knows about nodes — which types exist, their categories, their properties with
declared ranges, the port layout in serialization order, the modifiers Gaea attaches by itself
— is extracted from `Gaea.Nodes.dll` and from the `.terrain` files Gaea ships as examples. A
project it writes opens in the installed build, and a node type it does not recognise is
refused up front instead of producing a file Gaea rejects as corrupt.

Verified against **Gaea 2.3.0.1** on Windows.

## What it knows

Pulled from the installation by the generator, not written by hand:

| | |
|---|---|
| Node classes | 196, of which 193 carry a tool box category and a family |
| Modifier classes | 19, with the 7 that read the parent node's input marked |
| Enumerations | 144, with the members a property has to be written as |
| Properties | per node and per modifier: declared range, default — including enum and boolean defaults — the interface label where it differs from the serialized name, and the curve exponent where the value does not move linearly |
| Nodes needing baking | 48, marked `[RequiresBaking]`: built alone they can finish without writing a file and without an error |
| Search words | 64 nodes answer to words that are not their name — `Dusting` to "snow", `Glacier` to "ice", `Heal` to "reconstruct" |
| Short codes | 174, the codes Gaea itself uses |
| Port layouts | 106 node types, in the order Gaea serializes them |

And from the 59 scenes Gaea ships, what authors actually do rather than what the schema
permits: how often each node is used, the low, median and high value chosen for every property
they set, and 417 distinct connections ranked by frequency, which answers "what usually
follows this node".

That last part matters more than it sounds. A declared range of `0.0001..3` says nothing about
where to aim; the shipped scenes say `Mountain.Height` is set between 2.09 and 3.0, median
2.51. A caller reaching for a value in the middle of the declared range lands nowhere near
what anyone has ever used.

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

**Building** — `run_gaea2_project`, `analyze_gaea2_build`, `analyze_gaea2_terrain`,
`analyze_execution_history`.

Four of these are worth calling out:

* `gaea2_node_info` answers "what is this node and what can I set on it" from the installed
  build: category and family, ports in serialization order, intrinsic modifiers, properties
  with defaults, ranges and curves, whether the node needs baking, how often the shipped
  scenes use it, what usually feeds it and what it usually feeds. Give it a `node_type`, or
  `search`/`category` to list — search covers Gaea's own keywords, so "snow" finds `Dusting`.
* `analyze_gaea2_build` reads a build directory and reports the first faulting node from
  `CRASH_LOG.txt`. Only the first fault is informative — everything downstream of it reports
  that its input returned no data. It also summarises the terrain that was produced.
* `analyze_gaea2_terrain` opens a built heightfield — 16-bit raw or PNG, palette included —
  and describes the land rather than the file: relief in metres, how much of the playable
  middle is gentle enough to build on, whether the edges stand above that middle, the split
  into height classes, and a profile along each axis.
* `patch_gaea2_project` edits node properties in place, checking names against the installed
  build and values against their range, instead of regenerating a graph and losing whatever
  the caller did not restate.

## Judging a build by what it did

A Gaea build can exit cleanly having written nothing, so the server does not take the exit
code as the verdict:

* Before starting, it refuses a project with no `BuildDefinition.Type` and one where no node
  has an enabled `SaveDefinition` — both produce an empty directory and no error.
* Afterwards, it decides by the files on disk and their write times, so a rebuild that
  overwrites the same names counts as work rather than reading as an empty result.
* When a build writes nothing, it names any nodes in the graph marked `[RequiresBaking]`,
  a known way to get an empty result out of a graph that looks complete.
* When files are there, it reads the heightfield back and reports what the terrain is.

## Notes on Gaea itself

**Build resolution** must be a power of two from 256 to 8192. Gaea does not reject anything
else: it starts the build, works through the first node and exits without writing a file, so
an off-list value looks like a broken graph. Sizes such as 2049 belong to the `Unity` node's
`TargetSize`, not to the build resolution — the server refuses them with that explanation.

**Gaea.Swarm needs a real console.** Capturing its output hands it a pipe, and it dies with
`System.IO.IOException: The handle is invalid` (exit code -532462766) *after* the project has
loaded, which reads like a broken project. The Rust standard library always passes standard
handles when spawning, so the server calls `CreateProcessW` directly with `CREATE_NEW_CONSOLE`
and `SW_HIDE`: Gaea gets a console of its own, no window appears, and the real exit code comes
back. The crash log is read from the build directory afterwards.

**Output file names** come from the source node, not from `SaveDefinition.Filename`: a `Unity`
node fed by `Thermal2` writes `Thermal2_Out.raw`. That is Gaea's behaviour, not the server's.

**A property's value is not always its effect.** 42 properties carry a curve exponent, and
some scale with the size of what they make: a `Mountain` at `Scale 0.16` with `Height 0.42`
came out at 3% of full height and was invisible on the map. `gaea2_node_info` reports the
curve and what the shipped scenes chose, which is the fastest way to notice this before a
build rather than after one.

## Licence

[Unlicense](LICENSE), with the [MIT License](LICENSE-MIT) as a fallback for jurisdictions that
do not recognise a public domain dedication. Origin and attribution: [NOTICE.md](NOTICE.md).
