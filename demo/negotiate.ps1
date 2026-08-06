# Two autonomous agent loops resolving a conflict between themselves.
#
# `builder` is mid-task and holds computeFee(). `hotfix` needs the same symbol
# for a production fix. Nobody types anything: builder runs a policy loop that
# declines while it is busy and accepts once it is done, and hotfix retries
# until it is granted, then edits and verifies.

$ErrorActionPreference = 'Continue'
$demoDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Split-Path -Parent $demoDir
$atlas = Join-Path $repo 'target\debug\atlas.exe'
if (-not (Test-Path $atlas)) {
    Write-Error "build it first:  cargo build"
    exit 1
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("atlas-negotiate-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work | Out-Null
Copy-Item (Join-Path $demoDir 'src') (Join-Path $work 'src') -Recurse
Push-Location $work

function Say($text) { Write-Host "`n== $text" -ForegroundColor White }

try {
    & $atlas init | Out-Null
    & $atlas scan | Out-Null
    & $atlas --agent builder agent register builder --kind claude | Out-Null
    & $atlas --agent hotfix  agent register hotfix  --kind cursor | Out-Null

    Say 'builder starts work and takes the symbol'
    & $atlas --agent builder lease acquire computeFee --ttl 180 --task fees
    & $atlas --agent builder progress --percent 20 --note 'rewriting the fee table' --eta 6

    # Builder's policy loop, in its own process: decline while busy, accept when done.
    $builder = Start-Job -ArgumentList $atlas, $work -ScriptBlock {
        param($atlas, $work)
        Set-Location $work
        foreach ($pct in 40, 70, 100) {
            Start-Sleep -Seconds 2
            $eta = [int]((100 - $pct) / 20)
            & $atlas --agent builder progress --percent $pct --note "fee table $pct%" --eta $eta | Out-Null
            # Open requests print as "? <id> ...", and output is uncoloured when
            # captured, so the ids parse without extra tooling.
            $ids = & $atlas --agent builder request inbox |
                Where-Object { $_ -match '^\?\s+(\S+)' } |
                ForEach-Object { $Matches[1] }
            foreach ($id in $ids) {
                if ($pct -lt 100) {
                    & $atlas --agent builder request decline $id --reason "busy at $pct%, ~${eta}s left"
                } else {
                    & $atlas --agent builder request accept $id
                }
            }
        }
    }

    Start-Sleep -Seconds 1
    Say 'hotfix needs the same symbol'
    & $atlas --agent hotfix lease acquire computeFee --ttl 180 --no-queue *> $null
    if ($LASTEXITCODE -ne 0) {
        & $atlas --agent hotfix lease check computeFee
        foreach ($attempt in 1..5) {
            Say "hotfix asks (attempt $attempt)"
            & $atlas --agent hotfix request lease computeFee `
                --reason 'production hotfix' --priority 9 --deadline 12 --wait 12
            if ($LASTEXITCODE -eq 0) { break }
            Start-Sleep -Seconds 1
        }
    }

    # A request can also be fulfilled by the holder simply releasing, in which
    # case nobody handed us anything — make sure we actually hold it.
    & $atlas --agent hotfix lease acquire computeFee --ttl 180 --task hotfix | Out-Null

    Say 'hotfix edits the symbol it now owns'
    $payments = Join-Path $work 'src\payments.ts'
    $edited = [System.IO.File]::ReadAllText($payments).Replace(
        'return Math.round(amount * 0.029) + 30;',
        'return Math.round(amount * 0.019) + 25;')
    [System.IO.File]::WriteAllText($payments, $edited, (New-Object System.Text.UTF8Encoding $false))
    & $atlas --agent hotfix check
    & $atlas --agent hotfix lease release --all | Out-Null

    Wait-Job $builder | Out-Null
    Say "builder's side of the conversation"
    Receive-Job $builder
    Remove-Job $builder

    Say 'the negotiation, as recorded on the event bus'
    & $atlas watch --once --since 0 | Select-String -Pattern 'lease|request|progress'

    Say 'final state'
    & $atlas status --events 0
}
finally {
    Get-Job | Remove-Job -Force -ErrorAction SilentlyContinue
    Pop-Location
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
exit 0
