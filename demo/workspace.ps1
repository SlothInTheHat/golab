# The live collaborative workspace, end to end.
#
# Alice is on Cursor, Bob is on Claude Code. Both attach to the same golab
# workspace over MCP. Alice starts editing the payment API; Bob sees it before
# anything is committed. Alice changes a routed handler; Bob is notified that
# his work depends on it. Nothing here runs `git`.
#
# Ctrl-C to stop; the workspace is deleted.

$ErrorActionPreference = 'Continue'
$demoDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Split-Path -Parent $demoDir
$golab = Join-Path $repo 'target\debug\golab.exe'
if (-not (Test-Path $golab)) {
    Write-Error "build it first:  cargo build"
    exit 1
}

$port = if ($env:PORT) { $env:PORT } else { '7373' }
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("golab-ws-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work | Out-Null
Copy-Item (Join-Path $demoDir 'knowledge\*') $work -Recurse
Push-Location $work

function Say($text) { Write-Host "`n== $text" -ForegroundColor White }
function Note($text) { Write-Host "  $text" -ForegroundColor DarkGray }

# One long-lived `golab mcp` per tool -- an open editor is a process whose
# stdin stays open, and closing it is what quitting looks like.
$tools = @{}
function Start-Tool($name, $tool) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $golab
    # A single string rather than ArgumentList: this runs under Windows
    # PowerShell 5.1, whose .NET Framework has no ArgumentList.
    $psi.Arguments = "mcp --as $name --tool $tool"
    $psi.WorkingDirectory = $work
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $p = New-Object System.Diagnostics.Process
    $p.StartInfo = $psi

    $lines = New-Object System.Collections.Concurrent.ConcurrentQueue[string]
    Register-ObjectEvent -InputObject $p -EventName OutputDataReceived -MessageData $lines -Action {
        if ($EventArgs.Data) { $Event.MessageData.Enqueue($EventArgs.Data) }
    } | Out-Null
    $p.Start() | Out-Null
    $p.BeginOutputReadLine()

    # Our own writer over the raw stream: PowerShell 5.1's StandardInput emits
    # a UTF-8 BOM on its first write, which is not valid JSON.
    $stdin = New-Object System.IO.StreamWriter($p.StandardInput.BaseStream, (New-Object System.Text.UTF8Encoding($false)))
    $stdin.AutoFlush = $true
    $stdin.WriteLine('{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"' + $tool + '","version":"1.0"}}}')
    $stdin.WriteLine('{"jsonrpc":"2.0","method":"notifications/initialized"}')
    $tools[$name] = @{ proc = $p; lines = $lines; stdin = $stdin }
}

$script:id = 0
function Invoke-Tool($name, $toolName, $argsJson) {
    $script:id++
    $frame = '{"jsonrpc":"2.0","id":' + $script:id + ',"method":"tools/call","params":{"name":"' + $toolName + '","arguments":' + $argsJson + '}}'
    $tools[$name].stdin.WriteLine($frame)
}

# An editor hook callback, driven the way Claude Code drives one: the event
# payload on stdin. Returns the exit code, because a refused edit is a 2.
#
# Start-Process rather than a pipeline: redirecting a native command's stderr
# with `2>` inside PowerShell wraps every line in an ErrorRecord, so the
# refusal comes back decorated with a NativeCommandError that has nothing to do
# with what golab said.
function Invoke-Hook($agent, $event, $relPath, $callback) {
    $full = (Join-Path $work $relPath) -replace '\\', '/'
    $payload = '{"session_id":"s-' + $agent + '","cwd":"' + ($work -replace '\\', '/') +
               '","hook_event_name":"' + $event + '","tool_name":"Edit","tool_input":{"file_path":"' + $full + '"}}'
    $inFile = Join-Path $work 'hook.in'
    $errFile = Join-Path $work 'hook.err'
    $outFile = Join-Path $work 'hook.out'
    # No BOM: the payload is read as JSON, and 5.1's Set-Content would add one.
    [System.IO.File]::WriteAllText($inFile, $payload, (New-Object System.Text.UTF8Encoding($false)))

    $prev = $env:GOLAB_AGENT
    $env:GOLAB_AGENT = $agent
    $p = Start-Process -FilePath $golab -ArgumentList "hook $callback" -WorkingDirectory $work `
        -NoNewWindow -Wait -PassThru `
        -RedirectStandardInput $inFile -RedirectStandardOutput $outFile -RedirectStandardError $errFile
    $env:GOLAB_AGENT = $prev
    return @{ code = $p.ExitCode; reason = (Get-Content $errFile -Raw -ErrorAction SilentlyContinue) }
}

try {
    Say '1. a workspace, indexed'
    & $golab init | Out-Null
    & $golab index | Out-Null
    Note ((& $golab services | Measure-Object -Line).Lines.ToString() + ' services')

    Say '2. a goal, broken into work'
    # The scopes matter: bob's task covers a *test* that calls alice's handler,
    # so when alice changes it the runtime works out that bob is affected
    # without anyone having written that down.
    & $golab goal add 'Add refunds to the payment API' --priority 9 | Out-Null
    & $golab goal decompose G1 --task 'refund on the create path' --symbol createPayment --priority 9 | Out-Null
    & $golab goal decompose G1 --task 'cover it with tests' --symbol testCreatePayment --priority 8 | Out-Null
    & $golab task add 'ledger entries' --priority 3 --symbol record | Out-Null
    & $golab goal show G1 | Select-Object -First 6

    Say '3. alice opens Cursor, bob opens Claude Code'
    Start-Tool 'alice' 'cursor'
    Start-Tool 'bob' 'claude-code'
    Start-Sleep -Milliseconds 1200
    & $golab session list

    Say '4. work is handed out -- and its scope is leased in the same breath'
    & $golab assign T1 --to alice | Out-Null
    & $golab assign T2 --to bob | Out-Null
    & $golab swarm list

    Say '5. alice starts editing -- bob can see it before anything is committed'
    Note 'the pre-edit hook fires one keystroke before the change lands'
    Invoke-Hook 'alice' 'PreToolUse' 'api/src/routes.ts' 'guard' | Out-Null
    Invoke-Tool 'alice' 'progress' '{"percent":42,"note":"authorize() done, capture() next","eta_secs":300}'
    Start-Sleep -Milliseconds 800
    & $golab activity

    Say '6. bob reaches for the same file, and is refused'
    $r = Invoke-Hook 'bob' 'PreToolUse' 'api/src/routes.ts' 'guard'
    if ($r.code -eq 0) {
        Note 'allowed (alice must have finished)'
    } else {
        Note 'the edit was blocked -- this is the reason the model was given:'
        ($r.reason -split "`n") | ForEach-Object { if ($_.Trim()) { Write-Host "    $_" } }
    }

    Say "7. alice changes the endpoint's signature -- bob is told, unprompted"
    $routes = Join-Path $work 'api\src\routes.ts'
    (Get-Content $routes -Raw).Replace(
        'export function createPayment(req) {',
        'export function createPayment(req, idempotencyKey) {') |
        Set-Content $routes -Encoding utf8
    Invoke-Hook 'alice' 'PostToolUse' 'api/src/routes.ts' 'post-tool' | Out-Null
    & $golab scan api/src/routes.ts | Out-Null
    Start-Sleep -Milliseconds 800
    & $golab --agent bob request inbox | Select-Object -First 5

    Say '8. what a human sees'
    & $golab arch

    Write-Host ""
    Write-Host "  dashboard:  http://127.0.0.1:$port"
    Write-Host ""
    Write-Host "  what to look at, in order:"
    Write-Host "    - 'Live activity'  alice editing api/src/routes.ts, with a percentage"
    Write-Host "                       and an ETA, before any of it is committed"
    Write-Host "    - 'Notifications'  the api-change, saying which service depends on it"
    Write-Host "    - 'Repository'     the picture. Boxes somebody is inside are outlined"
    Write-Host "                       green with a pulsing dot; click one for who is in"
    Write-Host "                       it, what it depends on, and what it exposes"
    Write-Host "    - 'Workers'        filled dot = a coding tool is attached,"
    Write-Host "                       hollow = a bare CLI loop heartbeating"
    Write-Host ""
    Write-Host "  things to try:"
    Write-Host "    - click a notification: it selects the box the change landed in"
    Write-Host "    - switch the picture to '+ directories', drag to pan, scroll to zoom"
    Write-Host "    - press x on alice in 'Connected tools' -- her leases come straight back"
    Write-Host ""
    Write-Host "  no commits were needed for any of this to be visible."
    Write-Host "  ctrl-c to stop (the workspace is deleted)"
    Write-Host ""

    & $golab serve --port $port
}
finally {
    foreach ($t in $tools.Values) {
        try { $t.stdin.Close() } catch {}
        try { if (-not $t.proc.HasExited) { $t.proc.Kill() } } catch {}
    }
    Pop-Location
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
