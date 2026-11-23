# MSI lifecycle test (spec §3.3, §7) on a disposable Windows machine, from an elevated
# PowerShell. Runs every msiexec silently (/qn, verbose logs in -LogDir):
#   install old -> refuse -> upgrade to new -> downgrade refused -> uninstall -> reinstall
#   -> uninstall [-> purge]
# and asserts: the service is registered on the installed meshd.exe and runs as
# LocalService; `meshd --version`; the package marker; the admin SID is the installing user's
# (MESHNODE_ADMIN_USER defaults to [%USERDOMAIN]\[LogonUser]); the data directory's protected
# DACL; `meshd service install|uninstall` are refused; an upgrade stops the service before
# replacing the files and starts it again (a new process, Event Log "Meshnode stopped." then
# "Meshnode started"), keeping the node (same id), the admin SID and one installed product;
# the old MSI over the new one fails with the downgrade message and leaves the new one
# running; uninstall removes the service and C:\Program Files\Meshnode and keeps
# %ProgramData%\meshnode, which a reinstall picks up again (same node id).
#
#   test-msi.ps1 <old.msi> <new.msi> [-LogDir <dir>] [-Purge]
#
# "old" is any earlier build, e.g. one made after `scripts/set-version.sh 0.0.1` in a scratch
# copy of the repo. The test refuses to start while Meshnode is installed or
# %ProgramData%\meshnode exists (it needs a fresh node); -Purge deletes that directory at the
# end. Creates a node with `meshd init`. Exits 0 when every assertion passes.
param(
    [Parameter(Mandatory = $true)][string]$OldMsi,
    [Parameter(Mandatory = $true)][string]$NewMsi,
    [string]$LogDir = (Join-Path $env:TEMP 'meshnode-msi-test'),
    [switch]$Purge
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3

$conf = Get-Content -Raw (Join-Path $PSScriptRoot '..\apps\desktop\src-tauri\tauri.conf.json') | ConvertFrom-Json
$upgradeCode = '{' + $conf.bundle.windows.wix.upgradeCode.ToUpperInvariant() + '}'
$installDir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'Meshnode'
$meshd = Join-Path $installDir 'meshd.exe'
$data = Join-Path ([Environment]::GetFolderPath('CommonApplicationData')) 'meshnode'
$me = [System.Security.Principal.WindowsIdentity]::GetCurrent()
$mySid = $me.User.Value
$packageManaged = 'Meshnode is installed by the system package; use your package manager (or the Windows installer) instead'
$downgradeMessage = 'A newer version of Meshnode is already installed.'

$script:fails = 0
function Pass([string]$what) { Write-Output "ok   $what" }
function Fail([string]$what) { Write-Output "FAIL $what"; $script:fails++ }
function Check([string]$what, [bool]$ok) { if ($ok) { Pass $what } else { Fail $what } }

function Invoke-Com($obj, [string]$name, [string]$kind, [object[]]$argv) {
    $obj.GetType().InvokeMember($name, [System.Reflection.BindingFlags]$kind, $null, $obj, $argv)
}
$installer = New-Object -ComObject WindowsInstaller.Installer
# The installed products with Meshnode's UpgradeCode, as "productcode=version". Emits one
# string per product (nothing when none): callers collect them with @(Get-Products).
function Get-Products {
    # PowerShell enumerates the returned StringList: product codes, or nothing.
    $codes = @(Invoke-Com $installer 'RelatedProducts' 'GetProperty' @($upgradeCode) | Where-Object { $_ })
    foreach ($c in $codes) {
        $code = [string]$c
        "$code=$(Invoke-Com $installer 'ProductInfo' 'GetProperty' @($code, 'VersionString'))"
    }
}
function Get-MsiVersion([string]$msi) {
    $db = Invoke-Com $installer 'OpenDatabase' 'InvokeMethod' @($msi, 0)
    $view = Invoke-Com $db 'OpenView' 'InvokeMethod' @("SELECT ``Value`` FROM ``Property`` WHERE ``Property`` = 'ProductVersion'")
    Invoke-Com $view 'Execute' 'InvokeMethod' $null | Out-Null
    $rec = Invoke-Com $view 'Fetch' 'InvokeMethod' $null
    $v = Invoke-Com $rec 'StringData' 'GetProperty' @(1)
    Invoke-Com $view 'Close' 'InvokeMethod' $null | Out-Null
    return $v
}

# msiexec, silent, with a verbose log; returns its exit code.
function Invoke-Msiexec([string]$verb, [string]$target, [string]$log) {
    $p = Start-Process msiexec.exe -ArgumentList $verb, "`"$target`"", '/qn', '/norestart', '/l*v', "`"$log`"" -Wait -PassThru
    return $p.ExitCode
}
# meshd with its output (stdout and stderr) and exit code.
function Invoke-Meshd([string[]]$argv) {
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { $out = (& $meshd @argv 2>&1 | ForEach-Object { "$_" }) -join "`n" } finally { $ErrorActionPreference = $prev }
    return @{ Out = $out; Code = $LASTEXITCODE }
}
function Get-ServiceProcessId {
    $svc = Get-CimInstance Win32_Service -Filter "Name='meshnode'"
    if ($null -eq $svc) { return $null } else { return [int]$svc.ProcessId }
}
# Waits up to 30 s for the service to run and its admin pipe to answer `meshd status`.
function Wait-Running {
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $deadline) {
        $s = Get-Service meshnode -ErrorAction SilentlyContinue
        if ($null -ne $s -and $s.Status -eq 'Running' -and (Invoke-Meshd @('status')).Code -ne 3) { return $true }
        Start-Sleep -Milliseconds 500
    }
    return $false
}
function Get-NodeId {
    $line = ((Invoke-Meshd @('status')).Out -split "`n") | Where-Object { $_ -match '^Node:' } | Select-Object -First 1
    if ($null -eq $line) { return $null } else { return ($line -replace '^Node:\s*', '').Trim() }
}
# `meshd config get` prints `admin.windows_user_sid = <value>`.
function Get-AdminSid {
    $out = (Invoke-Meshd @('config', 'get', 'admin.windows_user_sid')).Out
    if ($out -match '(S-1-[0-9-]+)') { return $Matches[1] } else { return "(none: $out)" }
}
function Get-Events([datetime]$since) {
    return @(Get-WinEvent -FilterHashtable @{ LogName = 'Application'; ProviderName = 'Meshnode'; StartTime = $since } -ErrorAction SilentlyContinue |
        Sort-Object TimeCreated, RecordId | ForEach-Object { $_.Message })
}
# Whether `products` (Get-Products' strings) is exactly one product, at this version.
function Test-OneProduct([string[]]$products, [string]$version) {
    return ($products.Count -eq 1 -and $products[0] -match "^\{[0-9A-F-]+\}=$([regex]::Escape($version))$")
}
# The check must be able to fail: a side-by-side install, a wrong version, nothing.
foreach ($case in @(
        @(@('{A1B2C3D4-0000-0000-0000-000000000001}=0.1.0'), $true),
        @(@('{A1B2C3D4-0000-0000-0000-000000000001}=0.0.1', '{A1B2C3D4-0000-0000-0000-000000000002}=0.1.0'), $false),
        @(@('{A1B2C3D4-0000-0000-0000-000000000001}=0.0.1'), $false),
        @(@(), $false))) {
    if ((Test-OneProduct $case[0] '0.1.0') -ne $case[1]) { throw "Test-OneProduct self-check failed for ($($case[0] -join ', '))" }
}
function Check-OneProduct([string]$version) {
    $products = @(Get-Products)
    Check "one product installed, $version ($($products -join ', '))" (Test-OneProduct $products $version)
}
function Log-Has([string]$log, [string]$text) { return [bool](Select-String -Path $log -SimpleMatch $text -Quiet) }

# --- preconditions ------------------------------------------------------------------------
$principal = New-Object System.Security.Principal.WindowsPrincipal($me)
if (-not $principal.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'run this from an elevated PowerShell' }
$old = (Resolve-Path $OldMsi).Path
$new = (Resolve-Path $NewMsi).Path
$oldVersion = Get-MsiVersion $old
$newVersion = Get-MsiVersion $new
if (@(Get-Products).Count -gt 0) { throw "Meshnode is installed ($(@(Get-Products) -join ', ')); uninstall it first" }
if ($null -ne (Get-Service meshnode -ErrorAction SilentlyContinue)) { throw 'a meshnode service exists (a dev install?); remove it first' }
if (Test-Path $data) { throw "$data exists; this test needs a fresh node (delete it first)" }
New-Item -ItemType Directory -Force $LogDir | Out-Null
Write-Output "old $oldVersion ($old), new $newVersion ($new), logs in $LogDir"

# --- install old ----------------------------------------------------------------------------
$t0 = Get-Date
$code = Invoke-Msiexec '/i' $old (Join-Path $LogDir '1-install-old.log')
Check "install $oldVersion exits 0 (got $code)" ($code -eq 0)
Check-OneProduct $oldVersion
Check 'the service runs and answers' (Wait-Running)
$st = Invoke-Meshd @('service', 'status')
Check "meshd service status: installed, running ($($st.Out))" ($st.Out -eq 'meshnode service: installed, running')
Check "meshd --version is $oldVersion" ((Invoke-Meshd @('--version')).Out -eq "meshd $oldVersion")
Check 'the package marker is installed' (Test-Path (Join-Path $installDir 'packaged'))
$svc = Get-CimInstance Win32_Service -Filter "Name='meshnode'"
Check "the service runs the installed meshd.exe in place ($($svc.PathName))" ($svc.PathName -eq "`"$meshd`" run --service --data $data")
Check "the service runs as LocalService ($($svc.StartName))" ($svc.StartName -eq 'NT AUTHORITY\LocalService')
Check "the service starts automatically ($($svc.StartMode))" ($svc.StartMode -eq 'Auto')
$adminSid = Get-AdminSid
Check "the admin SID is the installing user's, $mySid (got $adminSid)" ($adminSid -eq $mySid)
$sddl = (Get-Acl $data).Sddl
Check "the data directory is owned by Administrators with a protected DACL for LocalService and Administrators ($sddl)" `
    ($sddl -match '^O:BA' -and $sddl -match 'D:PAI\(A;OICI;FA;;;LS\)\(A;OICI;FA;;;BA\)$')
$r = Invoke-Meshd @('service', 'install')
Check 'meshd service install is refused (package-managed)' ($r.Code -ne 0 -and $r.Out -match [regex]::Escape($packageManaged))
$r = Invoke-Meshd @('service', 'uninstall')
Check 'meshd service uninstall is refused (package-managed)' ($r.Code -ne 0 -and $r.Out -match [regex]::Escape($packageManaged))
$r = Invoke-Meshd @('init', '--name', 'msi-test')
Check "meshd init succeeds ($($r.Out -replace "`n", ' '))" ($r.Code -eq 0)
$nodeId = Get-NodeId
Check "meshd status shows the node ($nodeId)" ($null -ne $nodeId -and $nodeId -ne '')
Check 'Event Log: Meshnode started' (@(Get-Events $t0 | Where-Object { $_ -like 'Meshnode started*' }).Count -ge 1)

# --- upgrade to new -------------------------------------------------------------------------
$pidBefore = Get-ServiceProcessId
$t1 = Get-Date
$code = Invoke-Msiexec '/i' $new (Join-Path $LogDir '2-upgrade.log')
Check "upgrade to $newVersion exits 0 (got $code)" ($code -eq 0)
Check-OneProduct $newVersion
Check 'the service runs again and answers' (Wait-Running)
$pidAfter = Get-ServiceProcessId
Check "the service was restarted (process $pidBefore -> $pidAfter)" ($null -ne $pidAfter -and $pidAfter -ne 0 -and $pidAfter -ne $pidBefore)
Check "meshd --version is $newVersion" ((Invoke-Meshd @('--version')).Out -eq "meshd $newVersion")
$events = Get-Events $t1
$stopped = [array]::IndexOf($events, 'Meshnode stopped.')
$started = @(for ($i = 0; $i -lt $events.Count; $i++) { if ($events[$i] -like 'Meshnode started*') { $i } })
Check "Event Log: Meshnode stopped., then Meshnode started ($($events -join ' | '))" ($stopped -ge 0 -and @($started | Where-Object { $_ -gt $stopped }).Count -ge 1)
Check "the node is kept ($(Get-NodeId))" ((Get-NodeId) -eq $nodeId)
Check 'the admin SID is unchanged' ((Get-AdminSid) -eq $mySid)
Check 'the old product''s removal ran meshd service stop (MSI log)' (Log-Has (Join-Path $LogDir '2-upgrade.log') 'Stopped the meshnode service.')
Check 'the old product was removed without uninstalling the service' (-not (Log-Has (Join-Path $LogDir '2-upgrade.log') 'Removed the meshnode service'))

# --- downgrade --------------------------------------------------------------------------------
$pidBefore = Get-ServiceProcessId
$log = Join-Path $LogDir '3-downgrade.log'
$code = Invoke-Msiexec '/i' $old $log
Check "installing $oldVersion over $newVersion fails (exit $code)" ($code -ne 0)
Check "with the message: $downgradeMessage" (Log-Has $log $downgradeMessage)
Check-OneProduct $newVersion
Check 'the service was not touched (same process)' ((Get-ServiceProcessId) -eq $pidBefore)

# --- uninstall ------------------------------------------------------------------------------
function Test-Uninstall([string]$log) {
    $products = @(Get-Products)
    if ($products.Count -ne 1) { Fail "exactly one product to uninstall ($($products -join ', '))"; return }
    $product = ($products[0] -split '=')[0]
    $code = Invoke-Msiexec '/x' $product $log
    Check "uninstall exits 0 (got $code)" ($code -eq 0)
    Check 'no product installed' (@(Get-Products).Count -eq 0)
    Check 'meshd removed the service and kept the data (MSI log)' (Log-Has $log 'Removed the meshnode service; its data directory was kept.')
    Check 'the service is gone' ($null -eq (Get-Service meshnode -ErrorAction SilentlyContinue))
    Check "$installDir is gone" (-not (Test-Path $installDir))
    Check "$data is kept (node.key, meshd.toml)" ((Test-Path (Join-Path $data 'node.key')) -and (Test-Path (Join-Path $data 'meshd.toml')))
}
Test-Uninstall (Join-Path $LogDir '4-uninstall.log')

# --- reinstall ------------------------------------------------------------------------------
$code = Invoke-Msiexec '/i' $new (Join-Path $LogDir '5-reinstall.log')
Check "reinstall $newVersion exits 0 (got $code)" ($code -eq 0)
Check 'the service runs and answers' (Wait-Running)
Check "the kept node is used ($(Get-NodeId))" ((Get-NodeId) -eq $nodeId)
Check 'the admin SID is unchanged' ((Get-AdminSid) -eq $mySid)
Test-Uninstall (Join-Path $LogDir '6-uninstall.log')

if ($Purge) {
    # node.key grants LocalService only (spec §3.1: owner-only), so even an elevated
    # Administrator is denied until Administrators take ownership and grant themselves access.
    takeown /F $data /R /A /D Y | Out-Null
    icacls $data /grant '*S-1-5-32-544:(OI)(CI)F' /T /C /Q | Out-Null
    Remove-Item -Recurse -Force $data
    Check "purge: $data deleted" (-not (Test-Path $data))
}

if ($script:fails -gt 0) { Write-Output "$($script:fails) check(s) failed (logs in $LogDir)"; exit 1 }
Write-Output 'all checks passed'
exit 0
