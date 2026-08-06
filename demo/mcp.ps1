# Two coding tools sharing one repository through the MCP adapter.
#
# `alice` is on Claude Code, `bob` is on Cursor. Neither runs a lease command;
# both are driven entirely through the MCP tool surface, the way a real editor
# drives them. Both processes stay alive for the whole demo, because that is
# what an open editor is -- and it is what lets the runtime heartbeat them,
# renew their leases and hand back their work when they disconnect.
#
# What it shows is the part that used to need a human: bob is refused an edit
# alice owns, asks for it in one call, and is handed it the moment she accepts.

$ErrorActionPreference = 'Continue'
$demoDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Split-Path -Parent $demoDir
$golab = Join-Path $repo 'target\debug\golab.exe'
if (-not (Test-Path $golab)) {
    Write-Error "build it first:  cargo build"
    exit 1
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("golab-mcp-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work | Out-Null
Copy-Item (Join-Path $demoDir 'src') (Join-Path $work 'src') -Recurse
Push-Location $work

function Say($text) { Write-Host "`n== $text" -ForegroundColor White }
function Note($text) { Write-Host "  $text" -ForegroundColor DarkGray }

# One long-lived `golab mcp` per tool. Its stdin stays open for the whole demo,
# which is what an editor session is; closing it is what quitting looks like,
# and the server ends its session and hands its leases back.
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

    # Replies arrive asynchronously, because a synchronous read would block
    # whenever a step happens not to produce output.
    $lines = New-Object System.Collections.Concurrent.ConcurrentQueue[string]
    Register-ObjectEvent -InputObject $p -EventName OutputDataReceived -MessageData $lines -Action {
        if ($EventArgs.Data) { $Event.MessageData.Enqueue($EventArgs.Data) }
    } | Out-Null
    $p.Start() | Out-Null
    $p.BeginOutputReadLine()

    # Our own writer over the raw stream: PowerShell 5.1's StandardInput emits
    # a UTF-8 BOM on its first write, which is not valid JSON and produces a
    # parse error pointing at column 1 of a line that reads perfectly well.
    $stdin = New-Object System.IO.StreamWriter($p.StandardInput.BaseStream, (New-Object System.Text.UTF8Encoding($false)))
    $stdin.AutoFlush = $true

    # `initialize` is the whole registration story: by the time the first tool
    # call lands, the agent exists, holds a session and is being heartbeated.
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

# Print the text half of whatever arrived since last time. Every result carries
# both a compact summary and the full structured payload; the summary is what a
# model actually reads, so it is what this prints.
function Drain($name) {
    Start-Sleep -Milliseconds 700
    $q = $tools[$name].lines
    $line = $null
    while ($q.TryDequeue([ref]$line)) {
        try { $msg = $line | ConvertFrom-Json } catch { continue }
        if ($null -eq $msg.result -or $null -eq $msg.result.content) { continue }
        foreach ($c in $msg.result.content) {
            if ($c.text) { $c.text -split "`n" | ForEach-Object { Write-Host "  $_" } }
        }
    }
}

try {
    & $golab init | Out-Null
    & $golab index | Out-Null
    & $golab goal add 'Cut the payment fee' --priority 9 | Out-Null
    & $golab goal decompose G1 --task 'rework the fee table' --priority 9 --symbol computeFee | Out-Null

    Start-Tool 'alice' 'claude-code'
    Start-Tool 'bob' 'cursor'
    Start-Sleep -Milliseconds 1200

    Say 'alice opens Claude Code, bob opens Cursor'
    Note 'neither runs a golab command -- the handshake registered them both'
    & $golab session list

    Say 'alice is handed work'
    Invoke-Tool 'alice' 'next_task' '{}'
    Invoke-Tool 'alice' 'task_context' '{}'
    Drain 'alice'
    Note "-> she holds her task's scope, having asked for neither the task nor the lease"

    Say 'bob goes to edit the same file'
    Note 'the guard is asked before the edit, not after the commit'
    Invoke-Tool 'bob' 'check_edit' '{"path":"src/payments.ts"}'
    Drain 'bob'

    Say 'so bob asks for it -- one call, and he never names who to ask'
    Invoke-Tool 'bob' 'ask' '{"kind":"lease-transfer","symbol":"computeFee","reason":"production hotfix"}'
    Drain 'bob'
    $req = (& $golab --json request list | ConvertFrom-Json)[0].id
    Note "-> the runtime knew alice held it and addressed $req to her"

    Say 'alice hears about it on her next tool call, without asking'
    Note 'MCP has no way to push at a model, so notices ride along on every result'
    Invoke-Tool 'alice' 'whoami' '{}'
    Drain 'alice'

    Say 'she accepts; the symbol changes hands in that same transaction'
    Invoke-Tool 'alice' 'respond' ('{"request":"' + $req + '","action":"accept"}')
    Drain 'alice'
    & $golab lease list

    Say 'and now bob may edit it'
    Invoke-Tool 'bob' 'check_edit' '{"path":"src/payments.ts"}'
    Invoke-Tool 'bob' 'progress' '{"percent":80,"note":"lowering the fee"}'
    Drain 'bob'

    Say 'what a human sees'
    & $golab observe
    & $golab session list
    & $golab watch --once --since 0 | Select-String -Pattern 'session|lease|request' | Select-Object -Last 8

    Say 'the same workspace, live'
    Note 'golab serve   -> connected tools, who holds what, the critical path'
} finally {
    # Closing stdin is what quitting the editor looks like.
    foreach ($t in $tools.Values) {
        try { $t.stdin.Close() } catch {}
        try { $t.proc.WaitForExit(3000) | Out-Null } catch {}
        try { if (-not $t.proc.HasExited) { $t.proc.Kill() } } catch {}
    }
    Pop-Location
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
