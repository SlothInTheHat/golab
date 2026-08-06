# The everyday vocabulary end to end: a human states a goal, agents decompose
# and continue it, one submits for review and another approves, and a third
# gets work handed to it directly. Runs in a throwaway copy of demo/knowledge.

$ErrorActionPreference = 'Continue'
$demoDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Split-Path -Parent $demoDir
$golab = Join-Path $repo 'target\debug\golab.exe'
if (-not (Test-Path $golab)) {
    Write-Error "build it first:  cargo build"
    exit 1
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("golab-goals-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work | Out-Null
Copy-Item (Join-Path $demoDir 'knowledge\*') $work -Recurse
Push-Location $work

function Say($text) { Write-Host "`n== $text" -ForegroundColor White }
function Run() {
    Write-Host ('$ golab ' + ($args -join ' ')) -ForegroundColor DarkGray
    & $golab @args
}

try {
    Run init | Out-Null
    Run index | Out-Null

    Say '1. a human states a goal - not a lease, not a task list'
    Run goal add "Support voiding a payment" --priority 9

    Say '2. two agents join the swarm'
    Run --agent claude-1 swarm join claude-1 --kind claude
    Run --agent cursor-1 swarm join cursor-1 --kind cursor

    Say '3. decompose it - explicitly, or let the graph suggest where the work lands'
    Run goal decompose G1 --task "wire the void endpoint" --priority 9 --symbol voidPayment
    Run goal suggest G1 --near voidPayment --depth 2
    Write-Host '   -> advisory: names where the change touches, not what to do about it'

    Say '4. an agent asks to keep working - it gets handed the next startable task'
    Run --agent claude-1 continue --goal G1
    Write-Host "   -> the task's scope is leased in the same transaction it was claimed in"

    Say "5. one view of the whole swarm: who's doing what, what's blocked, what's idle"
    Run observe --goal G1

    Say '6. claude-1 finishes and submits for review - leases are kept, nothing merges yet'
    Run --agent claude-1 review submit T1
    Run lease list
    Write-Host '   -> still held: review is a checkpoint, not a handoff'

    Say "7. cursor-1 approves - leases release, and the goal's progress reflects it"
    Run --agent cursor-1 review approve T1
    Run goal show G1

    Say '8. a human reassigns the next task directly, no polling required'
    Run goal decompose G1 --task "ledger entry" --priority 5 --symbol record
    Run assign T2 --to cursor-1
    Run lease list

    Say '9. where the goal stands'
    Run goal show G1
}
finally {
    Pop-Location
    Remove-Item -Recurse -Force $work
}
