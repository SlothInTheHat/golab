# Walks through the whole runtime with two "agents" racing on one codebase.
# Runs in a throwaway copy of demo/src, so nothing here is mutated.

$ErrorActionPreference = 'Continue'
$demoDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Split-Path -Parent $demoDir
$golab = Join-Path $repo 'target\debug\golab.exe'
if (-not (Test-Path $golab)) {
    Write-Error "build it first:  cargo build"
    exit 1
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("golab-demo-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work | Out-Null
Copy-Item (Join-Path $demoDir 'src') (Join-Path $work 'src') -Recurse
Push-Location $work

function Say($text) { Write-Host "`n== $text" -ForegroundColor White }
function Run() {
    Write-Host ('$ golab ' + ($args -join ' ')) -ForegroundColor DarkGray
    & $golab @args
}

try {
    Say '1. index the repository into a symbol graph'
    Run init
    Run scan
    Run symbols --kind method

    Say '2. two agents join the workspace'
    Run --agent claude-1 agent register claude-1 --kind claude
    Run --agent cursor-1 agent register cursor-1 --kind cursor

    Say '3. claude-1 leases a function; cursor-1 wants the same one'
    Run --agent claude-1 lease acquire PaymentService.processPayment --ttl 300 --task stripe
    Run --agent cursor-1 lease acquire PaymentService.processPayment --ttl 300 --task refunds
    Write-Host '   -> denied, with the holder and the wait, not a merge conflict later'

    Say '4. disjoint work still runs in parallel'
    Run --agent cursor-1 lease acquire PaymentService.refund --ttl 300 --task refunds

    Say '5. leases nest: holding a class blocks its methods'
    Run --agent claude-1 lease check SessionStore.create
    Run --agent claude-1 lease acquire SessionStore --ttl 300
    Run --agent cursor-1 lease acquire SessionStore.create --ttl 300

    Say '6. enforcement: cursor-1 edits a function it does not hold'
    $payments = Join-Path $work 'src\payments.ts'
    # WriteAllText, not Set-Content: an accidental trailing-newline change is a
    # real file-level edit, and golab would (correctly) report it as one.
    $edited = [System.IO.File]::ReadAllText($payments).Replace(
        'const fee = computeFee(amount);',
        'const fee = computeFee(amount) * 2;')
    [System.IO.File]::WriteAllText($payments, $edited, (New-Object System.Text.UTF8Encoding $false))
    Run --agent cursor-1 check
    Write-Host "   -> exit $LASTEXITCODE (a pre-commit hook would stop here)"

    Say '7. the agent that holds the lease may make the very same edit'
    Run --agent claude-1 check

    Say "8. a crashed agent's lease expires on its own"
    Run --agent ghost lease acquire audit --ttl 2
    Write-Host '   (ghost dies without releasing; waiting for the TTL...)'
    Start-Sleep -Seconds 3
    Run --agent cursor-1 lease acquire audit --ttl 60
    Write-Host '   -> no operator, no deadlock: the lease simply timed out'

    Say '9. what else a change would touch, and who owns it'
    Run graph computeFee --depth 3

    Say '10. the task graph hands out only unblocked work'
    Run task add 'payment provider interface' --priority 5
    Run task add 'refund flow' --priority 9 --dep T1
    Run --agent cursor-1 task next

    Say '11. shared memory instead of re-deriving context every prompt'
    Run --agent claude-1 memory set decision/fees 'fees are basis points + 30c, computed in computeFee' --tag architecture
    Run memory list

    Say '12. the whole runtime at a glance'
    Run status --events 8

    Write-Host "`nlive dashboard:  golab serve   (http://127.0.0.1:7373)" -ForegroundColor DarkGray
}
finally {
    Pop-Location
    Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
exit 0
