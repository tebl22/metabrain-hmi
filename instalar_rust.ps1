# Installs MetaBrain (pure Rust). Preserves user data and config (%APPDATA%\MetaBrain).
param(
    [switch]$Force,
    [switch]$Lan,
    [int]$WaitSeconds = 0
)

$ErrorActionPreference = 'Stop'
$src  = $PSScriptRoot
$rel  = "$src\rust\target\release"
$dest = "$env:LOCALAPPDATA\Programs\MetaBrain"
$data = "$env:APPDATA\MetaBrain"
$st   = (Get-Date -f yyyyMMdd_HHmmss) + "_$PID"

function Get-MetaBrainPort {
    $cfgPath = Join-Path $data 'metabrain.json'
    if (Test-Path $cfgPath) {
        try {
            $cfg = Get-Content $cfgPath -Raw -Encoding UTF8 | ConvertFrom-Json
            if ($cfg.port -and [int]$cfg.port -gt 0 -and [int]$cfg.port -le 65535) {
                return [int]$cfg.port
            }
        } catch {}
    }
    return 8790
}

function Get-InstalledMetaBrain {
    Get-Process -Name metabrain, 'metabrain-hook' -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq "$dest\metabrain.exe" -or $_.Path -eq "$dest\metabrain-hook.exe" }
}

function Test-PreviousLanEnabled {
    try {
        $procs = Get-CimInstance Win32_Process -Filter "Name='metabrain.exe'" -ErrorAction SilentlyContinue
        foreach ($p in $procs) {
            if ($p.ExecutablePath -eq "$dest\metabrain.exe" -and $p.CommandLine -match '(^|\s)--lan(\s|$)') {
                return $true
            }
        }
    } catch {}
    return $false
}

$port = Get-MetaBrainPort
$preserveLan = $Lan -or (Test-PreviousLanEnabled)
$serveArgs = @('serve', '--no-browser')
if ($preserveLan) { $serveArgs += '--lan' }

$deadline = (Get-Date).AddSeconds([Math]::Max(0, $WaitSeconds))
$previous = @(Get-InstalledMetaBrain)
while ($previous.Count -gt 0 -and -not $Force) {
    if ((Get-Date) -ge $deadline) {
        throw "Close MetaBrain from the tray icon before updating (or pass -Force / -WaitSeconds)."
    }
    Start-Sleep -Seconds 1
    $previous = @(Get-InstalledMetaBrain)
}

# Prepare the complete installation next to the destination before touching it.
$stage = "$dest.stage.$st"
$backup = "$dest.previous.$st"
$failed = "$dest.failed.$st"
New-Item -ItemType Directory $stage -Force | Out-Null
Copy-Item "$rel\MetaBrain.exe" "$stage\metabrain.exe"
Copy-Item "$rel\metabrain-hook.exe" "$stage\metabrain-hook.exe"
Copy-Item "$src\metabrain\web" "$stage\web" -Recurse
Copy-Item "$src\build\metabrain.ico" "$stage\metabrain.ico"
if (Test-Path -LiteralPath $data) {
    Copy-Item -LiteralPath $data -Destination "$env:APPDATA\MetaBrain_respaldo_$st" -Recurse
    # Prune old data backups, keeping the 5 most recent.
    Get-ChildItem -LiteralPath $env:APPDATA -Directory -Filter 'MetaBrain_respaldo_*' -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending |
        Select-Object -Skip 5 |
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
}

$previous = @(Get-InstalledMetaBrain)
if ($previous.Count -gt 0 -and -not $Force) {
    throw "MetaBrain restarted during staging. Close it before updating."
}
foreach ($proc in $previous) {
    Stop-Process -InputObject $proc -Force -ErrorAction SilentlyContinue
    if (-not $proc.WaitForExit(5000)) { throw "MetaBrain process $($proc.Id) did not exit." }
}

$server = $null
$activated = $false
try {
    if (Test-Path -LiteralPath $dest) {
        [IO.Directory]::Move($dest, $backup)
    }
    [IO.Directory]::Move($stage, $dest)
    $activated = $true
    $server = Start-Process "$dest\metabrain.exe" -ArgumentList $serveArgs -WindowStyle Hidden -PassThru
    $ready = $false
    $readyBy = (Get-Date).AddSeconds(15)
    do {
        if ($server.HasExited) { throw "MetaBrain exited during startup." }
        $health = $null
        try {
            $health = Invoke-RestMethod -Uri "http://127.0.0.1:$port/api/health" -TimeoutSec 2
        } catch {}
        $ready = $health.pid -eq $server.Id
        if (-not $ready) { Start-Sleep -Milliseconds 250 }
    } until ($ready -or (Get-Date) -ge $readyBy)
    if (-not $ready) { throw "New instance did not confirm health on port $port." }
} catch {
    if ($server -and -not $server.HasExited) {
        Stop-Process -InputObject $server -Force -ErrorAction SilentlyContinue
        $null = $server.WaitForExit(5000)
    }
    if ($activated -and (Test-Path -LiteralPath $dest)) {
        try { [IO.Directory]::Move($dest, $failed) } catch { Remove-Item -LiteralPath $dest -Recurse -Force -ErrorAction SilentlyContinue }
    }
    if (Test-Path -LiteralPath $backup) {
        try {
            [IO.Directory]::Move($backup, $dest)
            if ($previous.Count -gt 0) {
                Start-Process "$dest\metabrain.exe" -ArgumentList $serveArgs -WindowStyle Hidden
            }
        } catch {
            Write-Warning "Rollback failed; previous install preserved at $backup"
        }
    }
    throw
}
"Installed MetaBrain v1.4.0 in $dest"

# Install Claude Code hooks (backing up configured Claude settings.json files) and register Windows autostart.
$claudeDirs = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
$null = $claudeDirs.Add((Join-Path $env:USERPROFILE '.claude'))
$cfgPath = Join-Path $data 'metabrain.json'
if (Test-Path -LiteralPath $cfgPath) {
    try {
        $cfgObj = Get-Content -LiteralPath $cfgPath -Raw -Encoding UTF8 | ConvertFrom-Json
        foreach ($b in @($cfgObj.brains)) {
            if ($b.kind -eq 'claude_code' -and $b.config_dir) {
                $cd = [string]$b.config_dir
                if ($cd.StartsWith('~/') -or $cd.StartsWith('~\')) {
                    $cd = Join-Path $env:USERPROFILE $cd.Substring(2)
                }
                $null = $claudeDirs.Add($cd)
            }
        }
    } catch {}
}
foreach ($dir in $claudeDirs) {
    $p = Join-Path $dir 'settings.json'
    if (Test-Path -LiteralPath $p) { Copy-Item -LiteralPath $p -Destination "$p.bak_hooks_$st" }
}
$hooksProc = Start-Process -FilePath "$dest\metabrain.exe" -ArgumentList 'hooks', 'install' -Wait -PassThru
if ($hooksProc.ExitCode -ne 0) { Write-Warning "hooks install returned exit code $($hooksProc.ExitCode)." }
$autoProc = Start-Process -FilePath "$dest\metabrain.exe" -ArgumentList 'autostart', 'on' -Wait -PassThru
$regAuto = (Get-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'MetaBrain' -ErrorAction SilentlyContinue).MetaBrain
if ($autoProc.ExitCode -ne 0 -or -not $regAuto) {
    throw "Could not register Windows autostart (code=$($autoProc.ExitCode))."
}

# Update Desktop and Start Menu shortcuts.
$shell = New-Object -ComObject WScript.Shell
$desktop = [Environment]::GetFolderPath('DesktopDirectory')
$startMenu = [Environment]::GetFolderPath('Programs')
$locations = @($desktop, $startMenu, "$env:USERPROFILE\OneDrive\Desktop") | Select-Object -Unique
foreach ($location in $locations) {
    if (-not $location -or -not (Test-Path -LiteralPath $location -PathType Container)) { continue }
    $lnk = Join-Path $location 'MetaBrain.lnk'
    if ($location -ne $desktop -and $location -ne $startMenu -and -not (Test-Path -LiteralPath $lnk)) { continue }
    $shortcut = $shell.CreateShortcut($lnk)
    $shortcut.TargetPath = "$dest\metabrain.exe"
    $shortcut.Arguments = ''
    $shortcut.WorkingDirectory = $dest
    $shortcut.IconLocation = "$dest\metabrain.ico,0"
    $shortcut.Save()
}
