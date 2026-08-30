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

# Modifiers attach to a node and are serialized with their own $type, so an invented name
# breaks the file the same way an invented node type does.
$modifierNames = New-Object System.Collections.Generic.List[string]
foreach ($full in ($baseOf.Keys | Sort-Object)) {
    if ($nsOf[$full] -ne 'QuadSpinner.Gaea.Nodes.Modifiers') { continue }
    if ($abstractOf[$full]) { continue }
    $modifierNames.Add(($full -split '\.')[-1])
}
Write-Host "Modifier classes: $($modifierNames.Count)"

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
# Class-level attributes the tool box itself reads: the family a node belongs to, the words it
# answers to in search, its short code, and whether it has to be baked before it yields anything.
$familyOf = @{}
$keywordsOf = @{}
$shortCodeOf = @{}
$requiresBaking = @{}

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

$isModifier = @{}
foreach ($n in $modifierNames) { $isModifier[$n] = $true }
$modifierPropsOf = @{}
$modifierUsesParentInput = @{}

$currentClass = $null
$currentIsModifier = $false
$pendingCat = $null
$pendingParam = $null
$pendingName = $null
$pendingFamily = $null
$pendingKeywords = $null
$pendingShortCode = $null
$pendingBaking = $false
$pendingCurve = $null

for ($i = 0; $i -lt $lines.Count; $i++) {
    $line = $lines[$i]

    if ($line -match '\[Toolbox\(NodeCategory\.([A-Za-z]+)') { $pendingCat = $Matches[1]; continue }
    if ($line -match '\[Family\("([^"]+)"\)\]') { $pendingFamily = $Matches[1]; continue }
    if ($line -match '\[ShortCode\("([^"]+)"\)\]') { $pendingShortCode = $Matches[1]; continue }
    if ($line -match '\[RequiresBaking\]') { $pendingBaking = $true; continue }
    if ($line -match '\[Keywords\(new string\[\]\s*\{([^}]*)\}') {
        $pendingKeywords = @()
        foreach ($m in [regex]::Matches($Matches[1], '"([^"]*)"')) { $pendingKeywords += $m.Groups[1].Value }
        continue
    }
    # A curved property does not move linearly under the slider, so a value read as "42% of the
    # range" is not what the node receives.
    if ($line -match '\[Curve\(([-0-9.]+)f?\)\]') { $pendingCurve = $Matches[1]; continue }
    if ($line -match '\[Parameter\((.*)\)\]') { $pendingParam = $Matches[1]; continue }
    # [Name("...")] on a property carries the label the user sees, which can differ from the
    # serialized name entirely: Multiplier.Value is shown as "Height Remap".
    if ($line -match '\[Name\("([^"]+)"\)\]') { $pendingName = $Matches[1]; continue }

    if ($line -match '^\s*(?:public|internal)\s+(?:sealed\s+)?(?:abstract\s+)?class\s+([A-Za-z0-9_]+)\s*:\s*([A-Za-z0-9_]+)') {
        $cls = $Matches[1]
        $baseName = $Matches[2]

        if ($baseName -eq 'Modifier' -and $isModifier.ContainsKey($cls)) {
            $currentClass = $cls
            $currentIsModifier = $true
            if (-not $modifierPropsOf.ContainsKey($cls)) {
                $modifierPropsOf[$cls] = New-Object System.Collections.Generic.List[object]
            }
        } elseif ($isNode.ContainsKey($cls)) {
            $currentClass = $cls
            $currentIsModifier = $false
            if ($pendingCat) { $catOf[$cls] = $pendingCat }
            if ($pendingFamily) { $familyOf[$cls] = $pendingFamily }
            if ($pendingShortCode) { $shortCodeOf[$cls] = $pendingShortCode }
            if ($pendingKeywords -and $pendingKeywords.Count -gt 0) { $keywordsOf[$cls] = $pendingKeywords }
            if ($pendingBaking) { $requiresBaking[$cls] = $true }
            if (-not $propsOf.ContainsKey($cls)) {
                $propsOf[$cls] = New-Object System.Collections.Generic.List[object]
            }
        } else {
            $currentClass = $null
            $currentIsModifier = $false
        }
        $pendingCat = $null; $pendingParam = $null; $pendingName = $null
        $pendingFamily = $null; $pendingKeywords = $null; $pendingShortCode = $null
        $pendingBaking = $false; $pendingCurve = $null
        continue
    }

    # A modifier whose Process reads base.Parent.In works off the node's input. Put one on a
    # generator, which has no input, and it yields nothing at all - Height on a Canyon made the
    # whole graph flat with no error anywhere.
    if ($currentClass -and $currentIsModifier -and $line -match 'base\.Parent\.In') {
        $modifierUsesParentInput[$currentClass] = $true
    }

    if ($currentClass -and $line -match '^\s*public\s+([A-Za-z0-9_<>\[\]\.\?]+)\s+([A-Za-z0-9_]+)\s*$') {
        $csType = $Matches[1]; $propName = $Matches[2]
        # "public enum X" lands here as a property named after a nested enum - skip it
        if ($csType -eq 'enum') { $pendingParam = $null; $pendingName = $null; continue }

        if (-not $currentIsModifier -and $propName -eq 'Seed' -and -not $seedNodes.Contains($currentClass)) {
            $seedNodes.Add($currentClass)
        }

        $def = $null; $min = $null; $max = $null; $defText = $null
        if ($pendingParam) {
            $parts = @($pendingParam -split ',\s*' | ForEach-Object { $_.Trim() })
            $nums = @()
            foreach ($a in $parts) {
                if ($a -match '^-?[0-9]+(\.[0-9]+)?f?$') { $nums += ($a -replace 'f$', '') }
            }

            # An enumerated or boolean property declares its default through a different overload:
            # the first argument names the editor to show, the second carries the value. Reading
            # only the numeric triple loses every enum default in the build.
            if ($parts.Count -ge 2 -and $parts[0] -match '^Parameters\.') {
                if ($parts[1] -match '^[A-Za-z0-9_]+\.([A-Za-z0-9_]+)$') { $defText = $Matches[1] }
                elseif ($parts[1] -in @('true', 'false')) { $defText = $parts[1] }
            } elseif ($csType -eq 'bool' -and $parts.Count -ge 1 -and $parts[0] -in @('true', 'false')) {
                $defText = $parts[0]
            }
            if ($csType -in @('float', 'int', 'double')) {
                # the canonical (default, min, max) triple; other overloads order them differently
                if ($nums.Count -eq 3 -and [double]$nums[1] -lt [double]$nums[2]) {
                    $def = $nums[0]; $min = $nums[1]; $max = $nums[2]
                }
            } elseif ($csType -eq 'Float2') {
                # a pair carries (defaultX, defaultY, min, max) for both components
                if ($nums.Count -eq 4 -and [double]$nums[2] -lt [double]$nums[3]) {
                    $min = $nums[2]; $max = $nums[3]
                }
            }
        }

        $record = [pscustomobject]@{
            Name = $propName; CsType = $csType; Default = $def; Min = $min; Max = $max
            Label = $pendingName; DefaultText = $defText; Curve = $pendingCurve
        }
        if ($currentIsModifier) { $modifierPropsOf[$currentClass].Add($record) }
        else { $propsOf[$currentClass].Add($record) }

        $pendingParam = $null; $pendingName = $null; $pendingCurve = $null
        continue
    }

    if ($line.Trim() -and $line -notmatch '^\s*\[') {
        $pendingCat = $null; $pendingParam = $null; $pendingName = $null
        $pendingFamily = $null; $pendingKeywords = $null; $pendingShortCode = $null
        $pendingBaking = $false; $pendingCurve = $null
    }
}
Write-Host "Modifiers reading the parent input: $($modifierUsesParentInput.Count)"
Write-Host "Categorized: $($catOf.Count); with Seed: $($seedNodes.Count)"
Write-Host "Families: $($familyOf.Count); keywords: $($keywordsOf.Count); short codes: $($shortCodeOf.Count); requiring baking: $($requiresBaking.Count)"

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

# What the shipped scenes actually do, as opposed to what the schema permits. The declared range
# of a property says 0.001..10; it does not say that every author who touched it stayed near 2.
# Without this, a caller setting a value has nothing to aim at but the midpoint of a range.
$usageCount = @{}          # node type -> how many times it appears
$propObserved = @{}        # node type -> property -> list of values seen
$edgeCount = @{}           # "FromType:FromPort->ToType:ToPort" -> how many times

# Bookkeeping that belongs to the node object itself, not to what the node computes.
$structuralKeys = @(
    'Id', 'Name', 'Version', 'Ports', 'Modifiers', 'NodeSize', 'Position', 'State',
    'ExportState', 'LastError', 'GraphIndex', 'IsExpanded', 'StateOverride', 'IgnoreUnderlay',
    'IsLocked', 'RenderIntentOverride', 'SaveDefinition', 'SaveDefinitions', 'Thumbprint',
    'Gizmos', 'Group', 'Metadata', 'BuildOrder'
)

function Read-Usage($obj, $nodesById, $records) {
    if ($null -eq $obj) { return }
    if ($obj -is [System.Object[]]) {
        foreach ($item in $obj) { Read-Usage $item $nodesById $records }
        return
    }
    if ($obj -isnot [System.Management.Automation.PSCustomObject]) { return }

    $props = $obj.PSObject.Properties
    $typeProp = $props | Where-Object { $_.Name -eq '$type' }
    if ($typeProp -and $typeProp.Value -is [string] -and
        $typeProp.Value -match '^QuadSpinner\.Gaea\.Nodes\.([A-Za-z0-9_]+), Gaea\.Nodes$') {
        $nodeType = $Matches[1]

        if (-not $usageCount.ContainsKey($nodeType)) { $usageCount[$nodeType] = 0 }
        $usageCount[$nodeType]++

        $idProp = $props | Where-Object { $_.Name -eq 'Id' }
        if ($idProp -and $null -ne $idProp.Value) { $nodesById[[string]$idProp.Value] = $nodeType }

        foreach ($p in $props) {
            if ($p.Name.StartsWith('$') -or $p.Name -in $structuralKeys) { continue }
            $v = $p.Value
            if ($v -is [System.Management.Automation.PSCustomObject] -or $v -is [System.Object[]]) { continue }
            if ($null -eq $v) { continue }
            if (-not $propObserved.ContainsKey($nodeType)) { $propObserved[$nodeType] = @{} }
            if (-not $propObserved[$nodeType].ContainsKey($p.Name)) {
                $propObserved[$nodeType][$p.Name] = New-Object System.Collections.Generic.List[object]
            }
            $propObserved[$nodeType][$p.Name].Add($v)
        }

        $portsProp = $props | Where-Object { $_.Name -eq 'Ports' }
        if ($portsProp -and $portsProp.Value) {
            $vals = $portsProp.Value.PSObject.Properties | Where-Object { $_.Name -eq '$values' }
            if ($vals -and $vals.Value) {
                foreach ($pt in $vals.Value) {
                    $rec = ($pt.PSObject.Properties | Where-Object { $_.Name -eq 'Record' }).Value
                    if (-not $rec) { continue }
                    $rp = $rec.PSObject.Properties
                    $from = ($rp | Where-Object { $_.Name -eq 'From' }).Value
                    $to = ($rp | Where-Object { $_.Name -eq 'To' }).Value
                    $fromPort = ($rp | Where-Object { $_.Name -eq 'FromPort' }).Value
                    $toPort = ($rp | Where-Object { $_.Name -eq 'ToPort' }).Value
                    if ($null -ne $from -and $null -ne $to) {
                        $records.Add([pscustomobject]@{
                            From = [string]$from; To = [string]$to
                            FromPort = $fromPort; ToPort = $toPort
                        })
                    }
                }
            }
        }
    }

    foreach ($p in $props) {
        $v = $p.Value
        if ($v -is [System.Management.Automation.PSCustomObject] -or $v -is [System.Object[]]) {
            Read-Usage $v $nodesById $records
        }
    }
}

if (Test-Path $examplesDir) {
    $sceneCount = 0
    foreach ($f in (Get-ChildItem $examplesDir -Filter '*.terrain' -Recurse)) {
        try { $json = [System.IO.File]::ReadAllText($f.FullName) | ConvertFrom-Json } catch { continue }
        Read-PortLayouts $json
        $sceneCount++

        # Connections name their endpoints by node id, so the types behind them are only known
        # once the whole scene has been walked.
        $nodesById = @{}
        $records = New-Object System.Collections.Generic.List[object]
        Read-Usage $json $nodesById $records
        foreach ($r in $records) {
            if (-not $nodesById.ContainsKey($r.From) -or -not $nodesById.ContainsKey($r.To)) { continue }
            $key = "$($nodesById[$r.From]):$($r.FromPort)->$($nodesById[$r.To]):$($r.ToPort)"
            if (-not $edgeCount.ContainsKey($key)) { $edgeCount[$key] = 0 }
            $edgeCount[$key]++
        }
    }
    Write-Host "Port layouts from examples: $($variants.Count) node types"
    Write-Host "Usage from $sceneCount scenes: $($usageCount.Count) node types, $($edgeCount.Count) distinct connections"
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

Add-Line "/// Modifier types a node can carry ($($modifierNames.Count))."
Add-Line 'pub static MODIFIER_TYPES: &[&str] = &['
foreach ($n in ($modifierNames | Sort-Object)) { Add-Line "    `"$n`"," }
Add-Line '];'
Add-Line ''

Add-Line '/// Modifiers whose Process reads the parent node''s input.'
Add-Line '///'
Add-Line '/// These work off what flows into the node: masks by height or slope, and combiners.'
Add-Line '/// Attached to a generator, which has no input, they produce nothing - and nothing in the'
Add-Line '/// build reports it. A Height modifier on a Canyon flattened an entire graph this way.'
Add-Line 'pub static MODIFIERS_USING_PARENT_INPUT: &[&str] = &['
foreach ($n in ($modifierUsesParentInput.Keys | Sort-Object)) { Add-Line "    `"$n`"," }
Add-Line '];'
Add-Line ''

Add-Line "/// Nodes exposing a Seed property ($($seedNodes.Count))."
Add-Line 'pub static SEEDED_NODES: &[&str] = &['
foreach ($n in ($seedNodes | Sort-Object)) { Add-Line "    `"$n`"," }
Add-Line '];'
Add-Line ''

Add-Line "/// Nodes that have to be baked before they yield anything ($($requiresBaking.Count))."
Add-Line '///'
Add-Line '/// Built on their own they can finish without writing a file and without an error, which'
Add-Line '/// reads as a broken graph rather than a node that needs the rest of the chain.'
Add-Line 'pub static NODES_REQUIRING_BAKING: &[&str] = &['
foreach ($n in ($requiresBaking.Keys | Sort-Object)) { Add-Line "    `"$n`"," }
Add-Line '];'
Add-Line ''

Add-Line "/// The family each node belongs to, a second axis beside the tool box category ($($familyOf.Count))."
Add-Line 'pub static NODE_FAMILY: &[(&str, &str)] = &['
foreach ($n in ($familyOf.Keys | Sort-Object)) { Add-Line "    (`"$n`", `"$($familyOf[$n])`")," }
Add-Line '];'
Add-Line ''

Add-Line "/// Short code Gaea itself uses for a node ($($shortCodeOf.Count))."
Add-Line 'pub static NODE_SHORT_CODES: &[(&str, &str)] = &['
foreach ($n in ($shortCodeOf.Keys | Sort-Object)) { Add-Line "    (`"$n`", `"$($shortCodeOf[$n])`")," }
Add-Line '];'
Add-Line ''

Add-Line "/// Search words a node answers to in the tool box ($($keywordsOf.Count))."
Add-Line '///'
Add-Line '/// Searching node names alone misses these: Dusting answers to "snow", Glacier to "ice",'
Add-Line '/// Hemisphere to "dome", Heal to "reconstruct".'
Add-Line 'pub static NODE_KEYWORDS: &[(&str, &[&str])] = &['
foreach ($n in ($keywordsOf.Keys | Sort-Object)) {
    $items = ($keywordsOf[$n] | ForEach-Object { "`"$_`"" }) -join ', '
    Add-Line "    (`"$n`", &[$items]),"
}
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

# what the shipped scenes do
Add-Line '/// How often each node appears across the scenes Gaea ships.'
Add-Line '///'
Add-Line '/// A node used in forty scenes is a staple; one used in a single scene is a specialist.'
Add-Line 'pub static NODE_USAGE: &[(&str, u32)] = &['
foreach ($n in ($usageCount.Keys | Sort-Object)) { Add-Line "    (`"$n`", $($usageCount[$n]))," }
Add-Line '];'
Add-Line ''

Add-Line '/// A property as the shipped scenes actually set it.'
Add-Line '#[derive(Debug, Clone, Copy, PartialEq)]'
Add-Line 'pub struct PropertyUsage {'
Add-Line '    /// Property name.'
Add-Line '    pub name: &''static str,'
Add-Line '    /// How many times an author set it explicitly.'
Add-Line '    pub times_set: u32,'
Add-Line '    /// Lowest value seen, for numeric properties.'
Add-Line '    pub low: Option<f64>,'
Add-Line '    /// Middle value seen, for numeric properties: what to aim at without other guidance.'
Add-Line '    pub median: Option<f64>,'
Add-Line '    /// Highest value seen, for numeric properties.'
Add-Line '    pub high: Option<f64>,'
Add-Line '    /// Most frequent value, for properties that are not numeric.'
Add-Line '    pub most_common: Option<&''static str>,'
Add-Line '}'
Add-Line ''

# Every number here has to be read and written in the invariant culture. On a machine whose
# locale writes a decimal comma, round-tripping a value through the current culture turns
# 0.178 into "0,178", which then fails to parse as a number and lands in the text branch -
# and would emit Rust that does not compile.
$invariant = [System.Globalization.CultureInfo]::InvariantCulture

function ConvertTo-InvariantNumber($v) {
    if ($v -is [bool]) { return $null }
    if ($v -is [double] -or $v -is [single] -or $v -is [int] -or $v -is [long] -or $v -is [decimal]) {
        return [double]$v
    }
    $d = 0.0
    if ([double]::TryParse([string]$v, [System.Globalization.NumberStyles]::Float, $invariant, [ref]$d)) {
        return $d
    }
    return $null
}

function Format-Usage($name, $values) {
    $times = $values.Count
    $nums = @()
    $allNumeric = $true
    foreach ($v in $values) {
        $n = ConvertTo-InvariantNumber $v
        if ($null -eq $n) { $allNumeric = $false; break }
        $nums += $n
    }

    if ($allNumeric -and $nums.Count -gt 0) {
        $sorted = $nums | Sort-Object
        $mid = $sorted[[int]([Math]::Floor($sorted.Count / 2))]
        $lo = $sorted[0]; $hi = $sorted[$sorted.Count - 1]
        $f = { param($x) ([Math]::Round($x, 4)).ToString($invariant) }
        return "PropertyUsage { name: `"$name`", times_set: $times, low: Some($(& $f $lo)_f64), median: Some($(& $f $mid)_f64), high: Some($(& $f $hi)_f64), most_common: None }"
    }

    $texts = $values | ForEach-Object {
        if ($_ -is [bool]) { if ($_) { 'true' } else { 'false' } }
        elseif ($_ -is [double] -or $_ -is [single] -or $_ -is [decimal]) { ([double]$_).ToString($invariant) }
        else { [string]$_ }
    }
    $top = $texts | Group-Object | Sort-Object -Property Count -Descending | Select-Object -First 1
    $tv = ([string]$top.Name).Replace('\', '\\').Replace('"', '\"')
    return "PropertyUsage { name: `"$name`", times_set: $times, low: None, median: None, high: None, most_common: Some(`"$tv`") }"
}

Add-Line '/// Per node, how the shipped scenes set its properties.'
Add-Line '///'
Add-Line '/// The declared range says what is allowed; this says what was chosen. Aiming at the middle'
Add-Line '/// of a declared range is how a caller lands on a value no author has ever used.'
Add-Line 'pub static PROPERTY_USAGE: &[(&str, &[PropertyUsage])] = &['
foreach ($t in ($propObserved.Keys | Sort-Object)) {
    $entries = @()
    foreach ($pn in ($propObserved[$t].Keys | Sort-Object)) {
        $entries += Format-Usage $pn $propObserved[$t][$pn]
    }
    if ($entries.Count -eq 0) { continue }
    Add-Line "    (`"$t`", &["
    foreach ($e in $entries) { Add-Line "        $e," }
    Add-Line '    ]),'
}
Add-Line '];'
Add-Line ''

Add-Line '/// Connections the shipped scenes make, most frequent first.'
Add-Line '///'
Add-Line '/// Reads as: this node, out of this port, into that node, at that port, this many times.'
Add-Line '/// Answers "what usually follows X", which no amount of schema can.'
Add-Line 'pub static COMMON_CONNECTIONS: &[(&str, &str, &str, &str, u32)] = &['
$ranked = $edgeCount.GetEnumerator() | Where-Object { $_.Value -ge 2 } | Sort-Object -Property Value -Descending
foreach ($e in $ranked) {
    if ($e.Key -notmatch '^([^:]+):([^-]*)->([^:]+):(.*)$') { continue }
    Add-Line "    (`"$($Matches[1])`", `"$($Matches[2])`", `"$($Matches[3])`", `"$($Matches[4])`", $($e.Value)),"
}
Add-Line '];'
Add-Line ''

# properties
Add-Line '/// A node or modifier property as declared by the installed build.'
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
Add-Line '    /// Label shown in the Gaea interface, when it differs from the serialized name.'
Add-Line '    /// Multiplier.Value, for one, is presented as "Height Remap".'
Add-Line '    pub label: Option<&''static str>,'
Add-Line '    /// Default for a property that is not numeric: an enumeration member, or true/false.'
Add-Line '    pub default_text: Option<&''static str>,'
Add-Line '    /// Curve exponent, when the property moves non-linearly under its slider. A curved'
Add-Line '    /// property set to half its range is not half its effect.'
Add-Line '    pub curve: Option<f64>,'
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

function Format-Property($p) {
    $dv = if ($null -ne $p.Default) { 'Some(' + $p.Default + '_f64)' } else { 'None' }
    $mnv = if ($null -ne $p.Min) { 'Some(' + $p.Min + '_f64)' } else { 'None' }
    $mxv = if ($null -ne $p.Max) { 'Some(' + $p.Max + '_f64)' } else { 'None' }
    $lbl = if ($p.Label) { 'Some("' + $p.Label + '")' } else { 'None' }
    $dt = if ($p.DefaultText) { 'Some("' + $p.DefaultText + '")' } else { 'None' }
    $cv = if ($null -ne $p.Curve) { 'Some(' + $p.Curve + '_f64)' } else { 'None' }
    "NodeProperty { name: `"$($p.Name)`", cs_type: `"$($p.CsType)`", default_value: $dv, min: $mnv, max: $mxv, label: $lbl, default_text: $dt, curve: $cv }"
}

Add-Line '/// Properties per node type.'
Add-Line 'pub static NODE_PROPERTIES: &[(&str, &[NodeProperty])] = &['
foreach ($k in ($propsOf.Keys | Sort-Object)) {
    if ($propsOf[$k].Count -eq 0) { continue }
    Add-Line "    (`"$k`", &["
    foreach ($p in $propsOf[$k]) { Add-Line "        $(Format-Property $p)," }
    Add-Line '    ]),'
}
Add-Line '];'
Add-Line ''

Add-Line '/// Properties per modifier type.'
Add-Line 'pub static MODIFIER_PROPERTIES: &[(&str, &[NodeProperty])] = &['
foreach ($k in ($modifierPropsOf.Keys | Sort-Object)) {
    Add-Line "    (`"$k`", &["
    foreach ($p in $modifierPropsOf[$k]) { Add-Line "        $(Format-Property $p)," }
    Add-Line '    ]),'
}
Add-Line '];'

$outFull = [System.IO.Path]::GetFullPath($OutFile)
[System.IO.File]::WriteAllText($outFull, $sb.ToString(), (New-Object System.Text.UTF8Encoding($false)))
Write-Host "Written: $outFull"
