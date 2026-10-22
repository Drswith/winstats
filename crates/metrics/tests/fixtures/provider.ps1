param([string]$Mode = 'async')
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
function Send($m) { [Console]::WriteLine((ConvertTo-Json -InputObject $m -Compress -Depth 8)) }
$hello = [Console]::ReadLine() | ConvertFrom-Json
if ($Mode -eq 'silent') { Start-Sleep -Seconds 30; exit }
Send @{t='hello';v=1}
Send @{t='describe';metrics=@(@{id='fast';name='Fast';ttl_ms=5000},@{id='slow';name='Slow';ttl_ms=5000},@{id='pid';name='Child';ttl_ms=5000})}
$sub = [Console]::ReadLine() | ConvertFrom-Json
if ($Mode -eq 'bad') { [Console]::WriteLine('invalid-json'); Start-Sleep -Seconds 30; exit }
if ($Mode -eq 'oversize') { [Console]::WriteLine(('x' * 70000)); Start-Sleep -Seconds 30; exit }
if ($Mode -eq 'crash') { exit 7 }
if ($Mode -eq 'tree') {
    $child = Start-Process -FilePath powershell.exe -ArgumentList @('-NoProfile','-Command','Start-Sleep -Seconds 60') -WindowStyle Hidden -PassThru
    Send @{t='sample';metric='pid';seq=1;sampled_at_ms=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds();value=$child.Id}
    Start-Sleep -Seconds 60
    exit
}
Send @{t='sample';metric='fast';seq=1;sampled_at_ms=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds();value=42}
if ($Mode -eq 'restart') { exit 7 }
Start-Sleep -Milliseconds 1100
Send @{t='sample';metric='slow';seq=1;sampled_at_ms=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds();value=99}
while ($true) { Send @{t='heartbeat'}; Start-Sleep -Milliseconds 300 }
