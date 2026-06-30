<#
.SYNOPSIS
  Kill the arc-runner that is LISTENING on a given port, leaving runners on
  other ports untouched.

.DESCRIPTION
  The stable (8787) and dev (8788) runners share the image name
  `arc-runner.exe`, so `taskkill /im arc-runner.exe` would kill BOTH. This
  targets by listening port instead, so the dev-loop rebuild
  (kill dev -> build-aware supervisor restarts it) never touches the stable
  runner. Defaults to the dev port (8788).

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File kill-runner.ps1 -Port 8788
#>
[CmdletBinding()]
param([int]$Port = 8788)

$conns = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue
if (-not $conns) { Write-Output "no listener on :$Port"; exit 0 }

$conns.OwningProcess | Sort-Object -Unique | ForEach-Object {
    $proc = Get-Process -Id $_ -ErrorAction SilentlyContinue
    if ($proc) {
        Write-Output ("killing PID {0} ({1}) on :{2}" -f $proc.Id, $proc.ProcessName, $Port)
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    }
}
