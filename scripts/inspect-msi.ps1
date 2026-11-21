# Checks a built Meshnode MSI (spec §3.3) without installing it, by reading its tables
# through the Windows Installer COM object (WindowsInstaller.Installer):
#   - UpgradeCode is bundle.windows.wix.upgradeCode and ProductVersion is the version in
#     apps/desktop/src-tauri/tauri.conf.json (scripts/check-version.sh keeps the others equal)
#   - meshd.exe and the package marker `packaged` are installed into INSTALLDIR, and
#     INSTALLDIR is pinned to [ProgramFiles64Folder]Meshnode\ (package_marker() and the
#     service's binary path assume it)
#   - the service custom actions run `meshd service ... --from-package` deferred and without
#     impersonation (as LocalSystem), with the admin user taken from [%USERDOMAIN]\[LogonUser],
#     and are scheduled with their conditions (install; stop + start around an upgrade;
#     uninstall, which keeps %ProgramData%\meshnode)
#   - a newer installed version blocks a downgrade with a message
#
#   inspect-msi.ps1 <meshnode.msi>
#
# Exits 0 when every check passes, 1 otherwise. Runs in Windows PowerShell 5.1 and pwsh.
param([Parameter(Mandatory = $true)][string]$Msi)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3

$conf = Get-Content -Raw (Join-Path $PSScriptRoot '..\apps\desktop\src-tauri\tauri.conf.json') | ConvertFrom-Json
$version = $conf.version
$upgradeCode = $conf.bundle.windows.wix.upgradeCode
if (-not $upgradeCode) { throw 'tauri.conf.json has no bundle.windows.wix.upgradeCode' }
$msiPath = (Resolve-Path $Msi).Path

$script:fails = 0
function Pass([string]$what) { Write-Output "ok   $what" }
function Fail([string]$what) { Write-Output "FAIL $what"; $script:fails++ }
function Check([string]$what, [bool]$ok) { if ($ok) { Pass $what } else { Fail $what } }

# The COM object has no type library PowerShell can bind to, hence InvokeMember.
function Invoke-Com($obj, [string]$name, [string]$kind, [object[]]$argv) {
    $flags = [System.Reflection.BindingFlags]$kind
    $obj.GetType().InvokeMember($name, $flags, $null, $obj, $argv)
}
$installer = New-Object -ComObject WindowsInstaller.Installer
# 0: msiOpenDatabaseModeReadOnly
$db = Invoke-Com $installer 'OpenDatabase' 'InvokeMethod' @($msiPath, 0)

# Rows of a query, each a hashtable keyed by the selected column names.
function Get-Rows([string]$table, [string[]]$columns) {
    $sql = 'SELECT ' + (($columns | ForEach-Object { "``$_``" }) -join ', ') + " FROM ``$table``"
    try { $view = Invoke-Com $db 'OpenView' 'InvokeMethod' @($sql) } catch { return @() }
    Invoke-Com $view 'Execute' 'InvokeMethod' $null | Out-Null
    $rows = @()
    while ($true) {
        $rec = Invoke-Com $view 'Fetch' 'InvokeMethod' $null
        if ($null -eq $rec) { break }
        $row = @{}
        for ($i = 0; $i -lt $columns.Count; $i++) {
            $row[$columns[$i]] = Invoke-Com $rec 'StringData' 'GetProperty' @($i + 1)
        }
        $rows += $row
    }
    Invoke-Com $view 'Close' 'InvokeMethod' $null | Out-Null
    return $rows
}
function Norm-Guid([string]$g) { $g.Trim('{', '}').ToUpperInvariant() }

$props = @{}
foreach ($r in Get-Rows 'Property' @('Property', 'Value')) { $props[$r.Property] = $r.Value }
$actions = @{}
foreach ($r in Get-Rows 'CustomAction' @('Action', 'Type', 'Source', 'Target')) { $actions[$r.Action] = $r }
$sequence = @{}
foreach ($r in Get-Rows 'InstallExecuteSequence' @('Action', 'Condition', 'Sequence')) { $sequence[$r.Action] = $r }
$files = Get-Rows 'File' @('File', 'Component_', 'FileName')
$components = @{}
foreach ($r in Get-Rows 'Component' @('Component', 'Directory_')) { $components[$r.Component] = $r.Directory_ }

# --- identity -----------------------------------------------------------------------------
Check "UpgradeCode is $upgradeCode (got $($props['UpgradeCode']))" `
    ($props.ContainsKey('UpgradeCode') -and (Norm-Guid $props['UpgradeCode']) -eq (Norm-Guid $upgradeCode))
Check "ProductVersion is $version (got $($props['ProductVersion']))" ($props['ProductVersion'] -eq $version)

# --- files --------------------------------------------------------------------------------
function Installed-In-InstallDir([string]$name) {
    foreach ($f in $files) {
        $long = ($f.FileName -split '\|')[-1]
        if ($long -eq $name -and $components[$f.Component_] -eq 'INSTALLDIR') { return $true }
    }
    return $false
}
Check 'meshd.exe is installed into INSTALLDIR' (Installed-In-InstallDir 'meshd.exe')
Check 'the package marker `packaged` is installed into INSTALLDIR' (Installed-In-InstallDir 'packaged')

# SetDirectory: a type-51 (or 35) action setting INSTALLDIR, in the execute sequence.
$pin = $actions.Values | Where-Object { $_.Source -eq 'INSTALLDIR' -and $_.Target -eq '[ProgramFiles64Folder]Meshnode\' }
Check 'INSTALLDIR is pinned to [ProgramFiles64Folder]Meshnode\' `
    ($null -ne $pin -and @($pin | Where-Object { $sequence.ContainsKey($_.Action) }).Count -gt 0)

# --- service custom actions ---------------------------------------------------------------
# msidbCustomActionType: 1 = DLL in the Binary table, 0x400 = in-script (deferred),
# 0x800 = no impersonation, 0x100 + 0x400 = rollback, 0x40 = ignore the exit code.
$deferredNoImpersonate = 0x400 -bor 0x800
$exe = '"[INSTALLDIR]meshd.exe"'
$adminValue = '[%USERDOMAIN]\[LogonUser]'
$expected = [ordered]@{
    # id = (command line, rollback?, condition)
    MeshnodeServiceStopRollback    = @("$exe service start", $true, 'MESHNODE_SERVICE AND Installed AND (UPGRADINGPRODUCTCODE OR NOT REMOVE~="ALL")')
    MeshnodeServiceStop            = @("$exe service stop", $false, 'MESHNODE_SERVICE AND Installed AND (UPGRADINGPRODUCTCODE OR NOT REMOVE~="ALL")')
    MeshnodeServiceUninstall       = @("$exe service uninstall --from-package", $false, 'MESHNODE_SERVICE AND REMOVE~="ALL" AND NOT UPGRADINGPRODUCTCODE')
    MeshnodeServiceInstallRollback = @("$exe service uninstall --from-package", $true, 'NOT MESHNODE_SERVICE AND NOT REMOVE~="ALL"')
    MeshnodeServiceInstall         = @("$exe service install --from-package --admin-user `"[MESHNODE_ADMIN_USER]`"", $false, 'NOT MESHNODE_SERVICE AND NOT REMOVE~="ALL"')
    MeshnodeServiceStart           = @("$exe service start", $false, 'MESHNODE_SERVICE AND NOT REMOVE~="ALL"')
}
foreach ($id in $expected.Keys) {
    $cmd, $rollback, $cond = $expected[$id]
    $ca = $actions[$id]
    if ($null -eq $ca) { Fail "custom action $id exists"; continue }
    $type = [int]$ca.Type
    $okType = ($type -band 0x3F) -eq 1 -and $ca.Source -eq 'WixCA' -and $ca.Target -eq 'WixQuietExec64' -and `
        ($type -band $deferredNoImpersonate) -eq $deferredNoImpersonate -and ((($type -band 0x100) -ne 0) -eq $rollback)
    Check "$id is a deferred, non-impersonated WixQuietExec64$(if ($rollback) { ' rollback' }) action (type $type)" $okType
    # Its command line is the CustomActionData set by an immediate SetProperty action.
    $setter = $actions.Values | Where-Object { ([int]$_.Type -band 0x3F) -eq 51 -and $_.Source -eq $id }
    Check "$id runs: $cmd" ($null -ne $setter -and @($setter | Where-Object { $_.Target -eq $cmd }).Count -eq 1)
    $seq = $sequence[$id]
    Check "$id is scheduled when: $cond" ($null -ne $seq -and $seq.Condition -eq $cond)
}
foreach ($id in 'MeshnodeServiceInstall', 'MeshnodeServiceUninstall') {
    $setter = $actions.Values | Where-Object { ([int]$_.Type -band 0x3F) -eq 51 -and $_.Source -eq $id }
    Check "$id passes --from-package" ($null -ne $setter -and $setter.Target -match '--from-package')
}

# Order: stop / uninstall before the files go, install / start after they are in place.
function Seq([string]$a) { if ($sequence.ContainsKey($a)) { [int]$sequence[$a].Sequence } else { -1 } }
Check 'stop and uninstall run before RemoveFiles' `
    ((Seq 'MeshnodeServiceStop') -gt 0 -and (Seq 'MeshnodeServiceStopRollback') -lt (Seq 'MeshnodeServiceStop') -and `
     (Seq 'MeshnodeServiceStopRollback') -gt 0 -and (Seq 'MeshnodeServiceUninstall') -gt 0 -and `
     (Seq 'MeshnodeServiceStop') -lt (Seq 'RemoveFiles') -and (Seq 'MeshnodeServiceUninstall') -lt (Seq 'RemoveFiles'))
Check 'install and start run after InstallFiles' `
    ((Seq 'MeshnodeServiceInstallRollback') -gt (Seq 'InstallFiles') -and (Seq 'InstallFiles') -gt 0 -and `
     (Seq 'MeshnodeServiceInstall') -gt (Seq 'MeshnodeServiceInstallRollback') -and `
     (Seq 'MeshnodeServiceStart') -gt (Seq 'InstallFiles') -and (Seq 'MeshnodeServiceStart') -lt (Seq 'InstallFinalize'))

# The admin user: the launching user, domain-qualified, set in both sequences unless given.
$adminSetters = $actions.Values | Where-Object { ([int]$_.Type -band 0x3F) -eq 51 -and $_.Source -eq 'MESHNODE_ADMIN_USER' }
Check "MESHNODE_ADMIN_USER is set to $adminValue" `
    ($null -ne $adminSetters -and @($adminSetters | Where-Object { $_.Target -eq $adminValue }).Count -ge 1)
$secure = if ($props.ContainsKey('SecureCustomProperties')) { $props['SecureCustomProperties'] -split ';' } else { @() }
Check 'MESHNODE_ADMIN_USER and MESHNODE_SERVICE are secure (passed to the server)' `
    (($secure -contains 'MESHNODE_ADMIN_USER') -and ($secure -contains 'MESHNODE_SERVICE'))
$search = Get-Rows 'RegLocator' @('Signature_', 'Root', 'Key', 'Name', 'Type')
Check 'MESHNODE_SERVICE comes from the service''s registry key' `
    (@($search | Where-Object { $_.Key -eq 'SYSTEM\CurrentControlSet\Services\meshnode' -and $_.Name -eq 'ImagePath' -and $_.Root -eq '2' }).Count -eq 1)

# --- downgrade ----------------------------------------------------------------------------
# MajorUpgrade with a DowngradeErrorMessage: an Upgrade row detecting newer versions
# (msidbUpgradeAttributesOnlyDetect = 2) into WIX_DOWNGRADE_DETECTED, and a launch condition
# that fails the install with the message when it is set.
$upgrade = Get-Rows 'Upgrade' @('UpgradeCode', 'VersionMin', 'Attributes', 'ActionProperty')
$detect = $upgrade | Where-Object {
    $_.ActionProperty -eq 'WIX_DOWNGRADE_DETECTED' -and (Norm-Guid $_.UpgradeCode) -eq (Norm-Guid $upgradeCode) -and `
    ([int]$_.Attributes -band 2) -ne 0 -and $_.VersionMin -eq $version }
$block = Get-Rows 'LaunchCondition' @('Condition', 'Description') | Where-Object { $_.Condition -eq 'NOT WIX_DOWNGRADE_DETECTED' }
Check 'a newer installed version is detected (downgrades are not allowed)' ($null -ne $detect)
Check "a detected downgrade fails with a message ($(if ($block) { $block.Description }))" `
    ($null -ne $block -and $block.Description -ne '' -and $null -ne $sequence['LaunchConditions'])

if ($script:fails -gt 0) { Write-Output "$($script:fails) check(s) failed"; exit 1 }
Write-Output 'all checks passed'
exit 0
