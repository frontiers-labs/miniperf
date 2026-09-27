param([Parameter(Mandatory = $true)][string]$PackageRoot)

# Native mperf emits useful notices on stderr; inspect its exit status below.
$ErrorActionPreference = 'Continue'
$mperf = Join-Path $PackageRoot 'bin/mperf.exe'
$gui = Join-Path $PackageRoot 'mperf-gui.exe'
$work = Join-Path (Get-Location) 'check-work'
$bin = Join-Path $work 'bin'
New-Item -ItemType Directory -Force -Path $bin | Out-Null
$fixture = Join-Path $bin 'windows_workload.exe'
$source = Join-Path $PSScriptRoot 'fixtures/windows_workload.c'

Push-Location $bin
try {
    & cl.exe /nologo /O2 "/Fe:$fixture" $source
    if ($LASTEXITCODE -ne 0) { throw 'could not compile the Windows workload' }
} finally {
    Pop-Location
}

$stat = & $mperf stat -e cpu_clock,page_faults -- $fixture 2>&1
if ($LASTEXITCODE -ne 0) { throw "mperf stat failed: $stat" }
$statText = $stat -join "`n"
if ($statText -notmatch 'cpu_clock' -or $statText -notmatch 'page_faults') {
    throw "mperf stat omitted requested counters: $statText"
}

foreach ($scenario in @('snapshot', 'mem', 'roofline')) {
    $dir = Join-Path $work "rec-$scenario"
    & $mperf record -s $scenario -o $dir -- $fixture
    if ($LASTEXITCODE -ne 0) { throw "Windows $scenario recording failed" }
    if (-not (Test-Path (Join-Path $dir 'info.json'))) {
        throw "Windows $scenario recording has no info.json"
    }
    $tour = & $gui --tour $dir 2>&1
    if ($LASTEXITCODE -ne 0) { throw "GUI tour of $scenario failed: $tour" }
    $expected = switch ($scenario) {
        'snapshot' { 'Resources' }
        'mem' { 'Memory' }
        'roofline' { 'Roofline' }
    }
    if (($tour -join "`n") -notmatch "tour: rendered $expected") {
        throw "GUI tour of $scenario omitted the $expected tab: $tour"
    }
    Write-Output "Windows $scenario recording and GUI tour passed"
}
