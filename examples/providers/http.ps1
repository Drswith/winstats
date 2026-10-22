# Windows PowerShell 5.1+; stdout is exclusively UTF-8 NDJSON protocol traffic.
# Every target has one asynchronous request in flight; different targets run concurrently.
param(
    [Parameter(Mandatory = $true)][string]$TargetsFile,
    [ValidateRange(100, 120000)][int]$TimeoutMs = 3000
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Net.Http
$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::OutputEncoding = $utf8
$inputReader = New-Object System.IO.StreamReader([Console]::OpenStandardInput(), $utf8)
$client = New-Object System.Net.Http.HttpClient
$client.Timeout = [System.Threading.Timeout]::InfiniteTimeSpan
$targets = Get-Content -LiteralPath $TargetsFile -Raw | ConvertFrom-Json
$known = @{}
$catalog = @()
foreach ($target in $targets) {
    if ($target.id -notmatch '^[A-Za-z0-9_.-]+$' -or $known.ContainsKey($target.id)) { throw 'Target ids must be unique ASCII identifiers.' }
    $uri = [Uri]$target.url
    if ($uri.Scheme -notin @('http', 'https')) { throw 'Only HTTP(S) targets are supported.' }
    $known[$target.id] = $target
    $catalog += @{id = "$($target.id)/latency"; name = "$($target.name) latency"; unit = 'ms'; ttl_ms = 15000; min = 0}
    $catalog += @{id = "$($target.id)/up"; name = "$($target.name) reachable"; unit = ''; ttl_ms = 15000; value_type = 'boolean'; min = 0; max = 1}
}
function Send-Message($message) { [Console]::WriteLine((ConvertTo-Json -InputObject $message -Compress -Depth 8)) }
function Get-UnixMs { [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() }
$subscriptions = @{}
$active = @{}
$next = @{}
$seq = [long]0
$hello = $false
$lastHeartbeat = [DateTime]::UtcNow
$read = $inputReader.ReadLineAsync()
try {
    while ($true) {
        if ($read.IsCompleted) {
            $line = $read.GetAwaiter().GetResult()
            if ($null -eq $line) { break }
            $message = ConvertFrom-Json -InputObject $line
            switch ($message.t) {
                'hello' {
                    if ($hello -or $message.v -ne 1) { throw 'Unsupported handshake.' }
                    Send-Message @{t = 'hello'; v = 1}
                    Send-Message @{t = 'describe'; metrics = @($catalog)}
                    $hello = $true
                }
                'subscribe' {
                    if (-not $hello) { throw 'Handshake required.' }
                    $subscriptions = @{}
                    foreach ($sub in $message.metrics) { $subscriptions[$sub.metric] = [Math]::Max(250, [long]$sub.interval_ms) }
                    foreach ($id in @($active.Keys)) {
                        if (-not $subscriptions.ContainsKey("$id/latency") -and -not $subscriptions.ContainsKey("$id/up")) {
                            $active[$id].cancel.Cancel()
                        }
                    }
                }
                'stop' { return }
                default { throw 'Unknown host message.' }
            }
            $read = $inputReader.ReadLineAsync()
        }
        foreach ($id in @($active.Keys)) {
            $request = $active[$id]
            if (-not $request.task.IsCompleted) { continue }
            $ok = $false
            $reason = $null
            $response = $null
            try {
                $response = $request.task.GetAwaiter().GetResult()
                $ok = $response.IsSuccessStatusCode
                if (-not $ok) { $reason = "HTTP $([int]$response.StatusCode)" }
            } catch { $reason = $_.Exception.GetBaseException().Message }
            finally {
                $request.watch.Stop()
                if ($null -ne $response) { $response.Dispose() }
                $request.message.Dispose()
                $request.cancel.Dispose()
            }
            $at = Get-UnixMs
            foreach ($suffix in @('latency', 'up')) {
                $metric = "$id/$suffix"
                if (-not $subscriptions.ContainsKey($metric)) { continue }
                $seq++
                if ($suffix -eq 'up') {
                    Send-Message @{t = 'sample'; metric = $metric; seq = $seq; sampled_at_ms = $at; value = [bool]$ok}
                } elseif ($ok) {
                    Send-Message @{t = 'sample'; metric = $metric; seq = $seq; sampled_at_ms = $at; value = $request.watch.Elapsed.TotalMilliseconds}
                } else {
                    if ($reason.Length -gt 800) { $reason = $reason.Substring(0, 800) }
                    Send-Message @{t = 'sample'; metric = $metric; seq = $seq; sampled_at_ms = $at; value = $null; error = $reason}
                }
            }
            $active.Remove($id)
        }
        if ($hello) {
            foreach ($id in $known.Keys) {
                if ($active.ContainsKey($id)) { continue }
                $intervals = @(@("$id/latency", "$id/up") | Where-Object { $subscriptions.ContainsKey($_) } | ForEach-Object { $subscriptions[$_] })
                if ($intervals.Count -eq 0) { continue }
                $now = [DateTime]::UtcNow
                if ($next.ContainsKey($id) -and $now -lt $next[$id]) { continue }
                $interval = ($intervals | Measure-Object -Minimum).Minimum
                $next[$id] = $now.AddMilliseconds($interval)
                $cancel = New-Object System.Threading.CancellationTokenSource($TimeoutMs)
                $requestMessage = New-Object System.Net.Http.HttpRequestMessage([System.Net.Http.HttpMethod]::Get, [string]$known[$id].url)
                $watch = [Diagnostics.Stopwatch]::StartNew()
                $task = $client.SendAsync($requestMessage, [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead, $cancel.Token)
                $active[$id] = @{task = $task; cancel = $cancel; message = $requestMessage; watch = $watch}
            }
            if (([DateTime]::UtcNow - $lastHeartbeat).TotalSeconds -ge 1) {
                Send-Message @{t = 'heartbeat'}
                $lastHeartbeat = [DateTime]::UtcNow
            }
        }
        Start-Sleep -Milliseconds 20
    }
} finally {
    foreach ($request in $active.Values) { $request.cancel.Cancel(); $request.cancel.Dispose(); $request.message.Dispose() }
    $client.Dispose()
    $inputReader.Dispose()
}
