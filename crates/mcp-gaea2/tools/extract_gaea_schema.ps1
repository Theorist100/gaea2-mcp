<#
.SYNOPSIS
    Extracts the node schema of an installed Gaea build into src/gaea_schema_generated.rs.

.DESCRIPTION
    The hand-written schema that used to live in src/schema.rs was based on Gaea 2.2.6.0 and
    contained node types that do not exist in the shipped product, while missing real ones.
    Generating the schema from the installation removes the guesswork: node names, categories
    and properties come from Gaea.Nodes.dll, and default port layouts come from the .terrain
    files that Gaea itself ships as examples.

    Four sources are used:
      * Gaea.Nodes.dll metadata  - node classes (non-abstract descendants of Node) and properties
      * ILSpy decompilation      - [Toolbox(NodeCategory.X)] attributes and [Parameter(d,min,max)]
      * Examples\*.terrain       - the port list and port ORDER each node is serialized with
      * a small hand-maintained table below - intrinsic modifiers that Gaea expects on a node

    Requires ilspycmd (dotnet tool install -g ilspycmd) and PowerShell 7+.

.PARAMETER GaeaPath
    Gaea installation directory (the one containing Gaea.Nodes.dll).

.PARAMETER OutFile
    Destination .rs file.

.EXAMPLE
    pwsh tools/extract_gaea_schema.ps1
    pwsh tools/extract_gaea_schema.ps1 -GaeaPath 'D:\Gaea' -OutFile src/gaea_schema_generated.rs
#>
[CmdletBinding()]
param(
    [string]$GaeaPath = "$env:LOCALAPPDATA\Programs\Gaea 2.0",
    [string]$OutFile = (Join-Path $PSScriptRoot '..\src\gaea_schema_generated.rs'),
    [string]$WorkDir = (Join-Path ([System.IO.Path]::GetTempPath()) 'gaea-schema-extract')
)

$ErrorActionPreference = 'Stop'

$dll = Join-Path $GaeaPath 'Gaea.Nodes.dll'
if (-not (Test-Path $dll)) { throw "Gaea.Nodes.dll not found at '$dll'. Pass -GaeaPath." }

$exeForVersion = Join-Path $GaeaPath 'Gaea.Swarm.exe'
$gaeaVersion = if (Test-Path $exeForVersion) {
    (Get-Item $exeForVersion).VersionInfo.FileVersion
} else {
    (Get-Item $dll).VersionInfo.FileVersion
}

Write-Host "Gaea version: $gaeaVersion"
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null

# ---------------------------------------------------------------- 1. metadata: node classes

$fs = [System.IO.File]::OpenRead($dll)
$pe = New-Object System.Reflection.PortableExecutable.PEReader($fs)
$md = [System.Reflection.Metadata.PEReaderExtensions]::GetMetadataReader($pe)

$baseOf = @{}; $nsOf = @{}; $abstractOf = @{}
foreach ($h in $md.TypeDefinitions) {
    $t = $md.GetTypeDefinition($h)
    $ns = $md.GetString($t.Namespace)
    $n = $md.GetString($t.Name)
    $full = if ($ns) { "$ns.$n" } else { $n }

    $b = ''
    if (-not $t.BaseType.IsNil) {
        try {
            if ($t.BaseType.Kind -eq 'TypeReference') {
                $b = $md.GetString($md.GetTypeReference($t.BaseType).Name)
            } elseif ($t.BaseType.Kind -eq 'TypeDefinition') {
                $b = $md.GetString($md.GetTypeDefinition($t.BaseType).Name)
            }
        } catch { $b = '' }
    }
    $baseOf[$full] = $b
    $nsOf[$full] = $ns
    $abstractOf[$full] = $t.Attributes.ToString() -match 'Abstract'
}
$pe.Dispose(); $fs.Dispose()

$byShort = @{}
foreach ($k in $baseOf.Keys) { $byShort[($k -split '\.')[-1]] = $k }

function Test-DerivesFromNode([string]$full) {
    $cur = $full
    for ($i = 0; $i -lt 12; $i++) {
        $b = $baseOf[$cur]
        if (-not $b) { return $false }
        if ($b -eq 'Node') { return $true }
        if (-not $byShort.ContainsKey($b)) { return $false }
        $next = $byShort[$b]
        if ($next -eq $cur) { return $false }
        $cur = $next
    }
    return $false
}

$nodeNames = New-Object System.Collections.Generic.List[string]
foreach ($full in ($baseOf.Keys | Sort-Object)) {
    if ($nsOf[$full] -ne 'QuadSpinner.Gaea.Nodes') { continue }
    if ($abstractOf[$full]) { continue }
    if (-not (Test-DerivesFromNode $full)) { continue }
    $nodeNames.Add(($full -split '\.')[-1])
}
Write-Host "Node classes: $($nodeNames.Count)"

$isNode = @{}
foreach ($n in $nodeNames) { $isNode[$n] = $true }

# ------------------------------------------- 2. decompilation: categories and parameter ranges

$decompDir = Join-Path $WorkDir 'decomp'
$decompFile = Join-Path $decompDir 'Gaea.Nodes.decompiled.cs'
if (-not (Test-Path $decompFile)) {
    New-Item -ItemType Directory -Force -Path $decompDir | Out-Null
    Write-Host 'Decompiling Gaea.Nodes.dll (ilspycmd)...'
    & ilspycmd $dll -o $decompDir | Out-Null
    if (-not (Test-Path $decompFile)) { throw "Decompilation produced no output in '$decompDir'." }
}

$lines = [System.IO.File]::ReadAllLines($decompFile)
$catOf = @{}
$propsOf = @{}
$enumValues = @{}
$seedNodes = New-Object System.Collections.Generic.List[string]

# Enumerations first: a property typed by one of these must be written as the member NAME.
# Writing the ordinal instead makes Gaea fail to load the node, and the build then exits with
# code 1 having produced nothing and no crash log (Rivers.RiverValleyWidth = 0 did exactly that).
for ($i = 0; $i -lt $lines.Count; $i++) {
    if ($lines[$i] -notmatch '^\s*(?:public|internal)\s+enum\s+([A-Za-z0-9_]+)') { continue }
    $enumName = $Matches[1]
    $members = New-Object System.Collections.Generic.List[string]
    for ($j = $i + 1; $j -lt [Math]::Min($i + 200, $lines.Count); $j++) {
        $line = $lines[$j].Trim()
        if ($line -eq '}') { break }
        if ($line -match '^\[' -or $line -eq '{' -or $line -eq '') { continue }
        if ($line -match '^([A-Za-z_][A-Za-z0-9_]*)\s*(=.*)?,?$') { $members.Add($Matches[1]) }
    }
    if ($members.Count -gt 0) { $enumValues[$enumName] = $members }
}
Write-Host "Enumerations: $($enumValues.Count)"

$currentClass = $null
$pendingCat = $null
$pendingParam = $null

for ($i = 0; $i -lt $lines.Count; $i++) {
    $line = $lines[$i]

    if ($line -match '\[Toolbox\(NodeCategory\.([A-Za-z]+)') { $pendingCat = $Matches[1]; continue }
    if ($line -match '\[Parameter\((.*)\)\]') { $pendingParam = $Matches[1]; continue }

    if ($line -match '^\s*(?:public|internal)\s+(?:sealed\s+)?(?:abstract\s+)?class\s+([A-Za-z0-9_]+)\s*:') {
        $cls = $Matches[1]
        if ($isNode.ContainsKey($cls)) {
            $currentClass = $cls
            if ($pendingCat) { $catOf[$cls] = $pendingCat }
            if (-not $propsOf.ContainsKey($cls)) {
                $propsOf[$cls] = New-Object System.Collections.Generic.List[object]
            }
        } else {
            $currentClass = $null
        }
        $pendingCat = $null; $pendingParam = $null
        continue
    }

    if ($currentClass -and $line -match '^\s*public\s+([A-Za-z0-9_<>\[\]\.\?]+)\s+([A-Za-z0-9_]+)\s*$') {
        $csType = $Matches[1]; $propName = $Matches[2]
        # "public enum X" lands here as a property named after a nested enum - skip it
        if ($csType -eq 'enum') { $pendingParam = $null; continue }

        if ($propName -eq 'Seed' -and -not $seedNodes.Contains($currentClass)) {
            $seedNodes.Add($currentClass)
        }

        $def = $null; $min = $null; $max = $null
        if ($pendingParam -and $csType -in @('float', 'int', 'double')) {
            $nums = @()
            foreach ($a in ($pendingParam -split ',\s*')) {
                $a = $a.Trim()
                if ($a -match '^-?[0-9]+(\.[0-9]+)?f?$') { $nums += ($a -replace 'f$', '') }
            }
            # only the canonical (default, min, max) triple with a sane range is trusted;
            # other [Parameter] overloads put the numbers in a different order
            if ($nums.Count -eq 3 -and [double]$nums[1] -lt [double]$nums[2]) {
                $def = $nums[0]; $min = $nums[1]; $max = $nums[2]
            }
        }
        $propsOf[$currentClass].Add([pscustomobject]@{
                Name = $propName; CsType = $csType; Default = $def; Min = $min; Max = $max
            })
        $pendingParam = $null
        continue
    }

    if ($line.Trim() -and $line -notmatch '^\s*\[') { $pendingCat = $null; $pendingParam = $null }
}
Write-Host "Categorized: $($catOf.Count); with Seed: $($seedNodes.Count)"

# ------------------------------------------------- 3. examples: port layout in serialized order

$examplesDir = Join-Path $GaeaPath 'Examples'
$variants = @{}

function Get-NormalizedPortType($t) {
    if (-not $t) { return 'In' }
    # ", Required" is added by the writer when a connection exists; not part of the layout
    return ($t -replace ',\s*Required', '').Trim()
}

function Read-PortLayouts($obj) {
    if ($null -eq $obj) { return }
    if ($obj -is [System.Object[]]) {
        foreach ($item in $obj) { Read-PortLayouts $item }
        return
    }
    if ($obj -isnot [System.Management.Automation.PSCustomObject]) { return }

    $props = $obj.PSObject.Properties
    $typeProp = $props | Where-Object { $_.Name -eq '$type' }
    if ($typeProp -and $typeProp.Value -is [string] -and
        $typeProp.Value -match '^QuadSpinner\.Gaea\.Nodes\.([A-Za-z0-9_]+), Gaea\.Nodes$') {
        $nodeType = $Matches[1]
        $portsProp = $props | Where-Object { $_.Name -eq 'Ports' }
        if ($portsProp -and $portsProp.Value) {
            $vals = $portsProp.Value.PSObject.Properties | Where-Object { $_.Name -eq '$values' }
            if ($vals -and $vals.Value) {
                $seq = @()
                foreach ($p in $vals.Value) {
                    $pn = ($p.PSObject.Properties | Where-Object { $_.Name -eq 'Name' }).Value
                    $pt = ($p.PSObject.Properties | Where-Object { $_.Name -eq 'Type' }).Value
                    if ($pn) { $seq += "$pn`::$(Get-NormalizedPortType $pt)" }
                }
                if ($seq.Count -gt 0) {
                    $key = $seq -join '|'
                    if (-not $variants.ContainsKey($nodeType)) { $variants[$nodeType] = @{} }
                    if (-not $variants[$nodeType].ContainsKey($key)) { $variants[$nodeType][$key] = 0 }
                    $variants[$nodeType][$key]++
                }
            }
        }
    }

    foreach ($p in $props) {
        $v = $p.Value
        if ($v -is [System.Management.Automation.PSCustomObject] -or $v -is [System.Object[]]) {
            Read-PortLayouts $v
        }
    }
}

if (Test-Path $examplesDir) {
    foreach ($f in (Get-ChildItem $examplesDir -Filter '*.terrain' -Recurse)) {
        try { $json = [System.IO.File]::ReadAllText($f.FullName) | ConvertFrom-Json } catch { continue }
        Read-PortLayouts $json
    }
    Write-Host "Port layouts from examples: $($variants.Count) node types"
} else {
    Write-Warning "No Examples directory at '$examplesDir'; port table will be minimal."
}

# Nodes absent from the shipped examples but whose layout is known from projects Gaea itself
# saved. Keep this list short and only add entries verified against a Gaea-written file.
$portOverrides = @{
    'Unity' = @('In::PrimaryIn', 'Out::PrimaryOut')
}
foreach ($k in $portOverrides.Keys) {
    if (-not $variants.ContainsKey($k)) {
        $variants[$k] = @{ ($portOverrides[$k] -join '|') = 1 }
    }
}

# Modifiers Gaea attaches to a node by itself. A node serialized without them can fault at
# build time: Thermal2 without the intrinsic Max throws "Index was outside the bounds of the
# array". Verified against Examples\Complex Scene - Debris.terrain.
$intrinsicModifiers = @{
    'Thermal2' = @(@{ Type = 'Max'; Order = 66 })
}

# ------------------------------------------------------------------------ 4. emit Rust source

$sb = New-Object System.Text.StringBuilder
function Add-Line([string]$s = '') { [void]$sb.AppendLine($s) }

Add-Line '// @generated by tools/extract_gaea_schema.ps1 - do not edit by hand.'
Add-Line '//'
Add-Line "// Source: Gaea $gaeaVersion (Gaea.Nodes.dll metadata, ILSpy attributes, shipped Examples)."
Add-Line '// Re-run the generator after upgrading Gaea; see tools/extract_gaea_schema.ps1.'
Add-Line ''
Add-Line '/// Version of the Gaea installation this schema was extracted from.'
Add-Line "pub const GAEA_VERSION: &str = `"$gaeaVersion`";"
Add-Line ''

# categories
$byCat = @{}
$uncategorized = New-Object System.Collections.Generic.List[string]
foreach ($n in ($nodeNames | Sort-Object)) {
    if ($catOf.ContainsKey($n)) {
        $c = $catOf[$n]
        if (-not $byCat.ContainsKey($c)) { $byCat[$c] = New-Object System.Collections.Generic.List[string] }
        $byCat[$c].Add($n)
    } else {
        $uncategorized.Add($n)
    }
}

foreach ($c in ($byCat.Keys | Sort-Object)) {
    $upper = $c.ToUpperInvariant()
    Add-Line "/// $c nodes ($($byCat[$c].Count))."
    Add-Line "pub static ${upper}_NODES: &[&str] = &["
    foreach ($n in $byCat[$c]) { Add-Line "    `"$n`"," }
    Add-Line '];'
    Add-Line ''
}

Add-Line "/// Node classes without a [Toolbox] attribute: real types, hidden from the tool box ($($uncategorized.Count))."
Add-Line 'pub static UNCATEGORIZED_NODES: &[&str] = &['
foreach ($n in $uncategorized) { Add-Line "    `"$n`"," }
Add-Line '];'
Add-Line ''

Add-Line '/// Every node category, paired with its node list.'
Add-Line 'pub static NODE_CATEGORIES: &[(&str, &[&str])] = &['
foreach ($c in ($byCat.Keys | Sort-Object)) {
    Add-Line "    (`"$c`", $($c.ToUpperInvariant())_NODES),"
}
Add-Line '];'
Add-Line ''

Add-Line "/// Nodes exposing a Seed property ($($seedNodes.Count))."
Add-Line 'pub static SEEDED_NODES: &[&str] = &['
foreach ($n in ($seedNodes | Sort-Object)) { Add-Line "    `"$n`"," }
Add-Line '];'
Add-Line ''

# ports
Add-Line '/// Port layout, in the order Gaea serializes it. Order matters: nodes address their'
Add-Line '/// inputs positionally, so a reordered list can fault the build.'
Add-Line 'pub static NODE_PORTS: &[(&str, &[(&str, &str)])] = &['
foreach ($t in ($variants.Keys | Sort-Object)) {
    $best = $variants[$t].GetEnumerator() | Sort-Object -Property Value -Descending | Select-Object -First 1
    $items = @()
    foreach ($p in ($best.Key -split '\|')) {
        $parts = $p -split '::'
        $items += "(`"$($parts[0])`", `"$($parts[1])`")"
    }
    Add-Line "    (`"$t`", &[$($items -join ', ')]),"
}
Add-Line '];'
Add-Line ''

# intrinsic modifiers
Add-Line '/// Modifiers Gaea attaches to a node itself; omitting them can fault the build.'
Add-Line 'pub static INTRINSIC_MODIFIERS: &[(&str, &[(&str, i64)])] = &['
foreach ($k in ($intrinsicModifiers.Keys | Sort-Object)) {
    $items = @()
    foreach ($m in $intrinsicModifiers[$k]) { $items += "(`"$($m.Type)`", $($m.Order))" }
    Add-Line "    (`"$k`", &[$($items -join ', ')]),"
}
Add-Line '];'
Add-Line ''

# properties
Add-Line '/// A node property as declared by the installed build.'
Add-Line '#[derive(Debug, Clone, Copy, PartialEq)]'
Add-Line 'pub struct NodeProperty {'
Add-Line '    /// Property name as it appears in the serialized node.'
Add-Line '    pub name: &''static str,'
Add-Line '    /// Declared .NET type, useful for reporting and for enum-valued properties.'
Add-Line '    pub cs_type: &''static str,'
Add-Line '    /// Default from [Parameter], when the attribute carried a (default, min, max) triple.'
Add-Line '    pub default_value: Option<f64>,'
Add-Line '    /// Lower bound, when known.'
Add-Line '    pub min: Option<f64>,'
Add-Line '    /// Upper bound, when known.'
Add-Line '    pub max: Option<f64>,'
Add-Line '}'
Add-Line ''
Add-Line '/// Members of every enumeration a property can be typed by, in declaration order.'
Add-Line '///'
Add-Line '/// A property of an enumerated type has to be written as the member name. Writing the'
Add-Line '/// ordinal makes Gaea fail to load the node: the build exits 1 with no files and no crash'
Add-Line '/// log, which looks like a broken graph rather than a bad value.'
Add-Line 'pub static ENUM_VALUES: &[(&str, &[&str])] = &['
foreach ($k in ($enumValues.Keys | Sort-Object)) {
    $items = ($enumValues[$k] | ForEach-Object { "`"$_`"" }) -join ', '
    Add-Line "    (`"$k`", &[$items]),"
}
Add-Line '];'
Add-Line ''

Add-Line '/// Properties per node type.'
Add-Line 'pub static NODE_PROPERTIES: &[(&str, &[NodeProperty])] = &['
foreach ($k in ($propsOf.Keys | Sort-Object)) {
    if ($propsOf[$k].Count -eq 0) { continue }
    $items = @()
    foreach ($p in $propsOf[$k]) {
        $dv = if ($null -ne $p.Default) { 'Some(' + $p.Default + '_f64)' } else { 'None' }
        $mnv = if ($null -ne $p.Min) { 'Some(' + $p.Min + '_f64)' } else { 'None' }
        $mxv = if ($null -ne $p.Max) { 'Some(' + $p.Max + '_f64)' } else { 'None' }
        $items += "NodeProperty { name: `"$($p.Name)`", cs_type: `"$($p.CsType)`", default_value: $dv, min: $mnv, max: $mxv }"
    }
    Add-Line "    (`"$k`", &["
    foreach ($it in $items) { Add-Line "        $it," }
    Add-Line '    ]),'
}
Add-Line '];'

$outFull = [System.IO.Path]::GetFullPath($OutFile)
[System.IO.File]::WriteAllText($outFull, $sb.ToString(), (New-Object System.Text.UTF8Encoding($false)))
Write-Host "Written: $outFull"
