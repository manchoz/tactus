#Requires -Version 5.1
<#
.SYNOPSIS
  Thin Windows host launcher for the Tactus Arduino UNO Q release bundle.
.DESCRIPTION
  Guided (or automated with -Yes) installer that uploads this release bundle to an
  Arduino UNO Q over USB-C ADB or IPv4 SSH and invokes install-unoq-board.sh on-board.
#>
[CmdletBinding()]
param(
    [switch]$Adb,
    [string]$Serial = "",
    [string]$Ssh = "",
    [string]$Identity = "",
    [string]$PubKey = "",
    [string]$Hostname = "tactus",
    [string]$Psk = "",
    [string]$Country = "",
    [switch]$NoPasswdSudo,
    [switch]$SkipMcu,
    [switch]$SkipKernel,
    [switch]$NoTrim,
    [switch]$StageOnly,
    [switch]$RollbackAp,
    [switch]$Reboot,
    [switch]$NoReboot,
    [switch]$Yes,
    [switch]$Help
)

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path

function Show-Usage {
    @"
Usage:
  .\install.ps1 [OPTIONS]

Guided (or automated with -Yes) installer for an Arduino UNO Q from a Tactus release bundle.

Options:
  -Adb                 Use USB-C ADB transport
  -Serial ID           Optional adb device serial
  -Ssh USER@HOST       Use IPv4 SSH transport (e.g. arduino@tactus.local)
  -Identity FILE       SSH private key path (for -Ssh mode)
  -PubKey FILE         SSH public key to install into /home/arduino/.ssh/authorized_keys
  -Hostname NAME       Target hostname (default: tactus)
  -Psk PASSPHRASE      WPA2 passphrase for 'Tactus' WiFi AP (8..63 chars)
  -Country CC          2-letter uppercase ISO WiFi country code (e.g. IT, US, DE)
  -NoPasswdSudo        Configure passwordless sudo for user 'arduino'
  -SkipMcu             Skip flashing STM32U585 MCU LED matrix firmware
  -SkipKernel          Skip installing the custom 5 GHz WiFi kernel .deb
  -NoTrim              Keep GUI/PipeWire/extra Arduino services untouched
  -StageOnly           Stage configs without switching wlan0 to AP mode
  -RollbackAp          Restore wlan0 to NetworkManager and stop tactus-ap.service
  -Reboot              Reboot automatically when kernel update requires it
  -NoReboot            Do not prompt or reboot after installation
  -Yes                 Non-interactive mode using defaults and provided flags
  -Help                Show this help message
"@
}

function Write-Log([string]$Message) {
    Write-Host "[tactus] $Message"
}

function Fail([string]$Message) {
    Write-Error "[tactus] ERROR: $Message"
    exit 1
}

function Show-AdbInstallHint {
    Write-Host "'adb' (Android Platform Tools) is required for USB-C installation:" -ForegroundColor Yellow
    Write-Host "  winget install Google.PlatformTools"
}

function Read-PlainSecret([string]$PromptText) {
    $sec = Read-Host -Prompt $PromptText -AsSecureString
    $bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($sec)
    try {
        return [Runtime.InteropServices.Marshal]::PtrToStringAuto($bstr)
    } finally {
        [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
    }
}

function ConvertTo-ShSingleQuoted([string]$Value) {
    return "'" + ($Value -replace "'", "'`"`'`"`'") + "'"
}

function Invoke-Native([scriptblock]$Action) {
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        & $Action
    } finally {
        $ErrorActionPreference = $prevEap
    }
    return $LASTEXITCODE
}

if ($Help) {
    Show-Usage
    exit 0
}

if (-not (Test-Path (Join-Path $ScriptDir "system\scripts\install-unoq-board.sh"))) {
    Fail "Run install.ps1 from inside the extracted Tactus release bundle directory."
}

if ([string]::IsNullOrEmpty($Psk) -and $env:TACTUS_AP_PSK) {
    $Psk = $env:TACTUS_AP_PSK
}
if ([string]::IsNullOrEmpty($Country) -and $env:TACTUS_WIFI_COUNTRY) {
    $Country = $env:TACTUS_WIFI_COUNTRY
}

if ([string]::IsNullOrEmpty($PubKey)) {
    $ed = Join-Path $HOME ".ssh\id_ed25519.pub"
    $rsa = Join-Path $HOME ".ssh\id_rsa.pub"
    if (Test-Path $ed) { $PubKey = $ed }
    elseif (Test-Path $rsa) { $PubKey = $rsa }
}

$Transport = ""
if ($Adb) { $Transport = "adb" }
elseif (-not [string]::IsNullOrEmpty($Ssh)) { $Transport = "ssh" }

if ([string]::IsNullOrEmpty($Transport)) {
    $adbDetected = $false
    $adbCmd = Get-Command adb -ErrorAction SilentlyContinue
    if ($adbCmd) {
        $adbArgs = @()
        if ($Serial) { $adbArgs += @("-s", $Serial) }
        $adbArgs += "get-state"
        $prevEap = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        try {
            $state = (& adb @adbArgs 2>$null | Out-String).Trim()
        } finally {
            $ErrorActionPreference = $prevEap
        }
        if ($LASTEXITCODE -eq 0 -and $state -eq "device") { $adbDetected = $true }
    }

    if ($adbDetected) {
        Write-Log "Detected Arduino UNO Q over USB-C ADB."
        $Transport = "adb"
    } elseif ($Yes) {
        if (-not $adbCmd) { Show-AdbInstallHint }
        Fail "No UNO Q detected via USB-C ADB; pass -Ssh <user@host> or connect via USB-C."
    } else {
        Write-Host "No UNO Q detected over USB-C ADB."
        $choice = Read-Host "Choose connection method: [1] USB-C ADB (factory/offline)  [2] SSH (Ethernet/WiFi)"
        if ($choice -eq "2") {
            $Transport = "ssh"
            $Ssh = Read-Host "SSH target [arduino@tactus.local]"
            if ([string]::IsNullOrEmpty($Ssh)) { $Ssh = "arduino@tactus.local" }
        } else {
            if (-not $adbCmd) {
                Show-AdbInstallHint
                exit 1
            }
            $Transport = "adb"
        }
    }
}

if (-not $Yes -and -not $RollbackAp) {
    if ([string]::IsNullOrEmpty($Psk)) {
        $inPsk = Read-PlainSecret "WiFi AP passphrase for 'Tactus' [Enter = keep existing / default 'tactus-link']"
        if (-not [string]::IsNullOrEmpty($inPsk)) { $Psk = $inPsk }
    }
    if ([string]::IsNullOrEmpty($Country)) {
        $inCc = Read-Host "WiFi 2-letter country code [Enter = keep existing / default 'IT']"
        if (-not [string]::IsNullOrEmpty($inCc)) { $Country = $inCc.Trim().ToUpperInvariant() }
    }
    if (-not $SkipMcu) {
        $ansMcu = Read-Host "Flash STM32U585 MCU LED matrix firmware? [Y/n]"
        if ($ansMcu -match "^[Nn]$") { $SkipMcu = $true }
    }
    if (-not $SkipKernel) {
        $ansKern = Read-Host "Install custom 5 GHz WiFi kernel (with automatic +2 boot fallback)? [Y/n]"
        if ($ansKern -match "^[Nn]$") { $SkipKernel = $true }
    }
}

if (-not [string]::IsNullOrEmpty($Psk) -and ($Psk.Length -lt 8 -or $Psk.Length -gt 63)) {
    Fail "AP passphrase must be 8..63 characters."
}
if (-not [string]::IsNullOrEmpty($Country)) {
    $Country = $Country.Trim().ToUpperInvariant()
    if ($Country -notmatch "^[A-Z]{2}$") {
        Fail "Country code must be 2 uppercase letters (e.g. IT, US, DE)."
    }
}

if ($RollbackAp) {
    $BoardArgsStr = "--bundle-dir /tmp/tactus-install --rollback-ap"
} else {
    $BoardArgs = @("--bundle-dir", "/tmp/tactus-install", "--hostname", (ConvertTo-ShSingleQuoted $Hostname))
    if (-not [string]::IsNullOrEmpty($Psk)) { $BoardArgs += @("--psk", (ConvertTo-ShSingleQuoted $Psk)) }
    if (-not [string]::IsNullOrEmpty($Country)) { $BoardArgs += @("--country", (ConvertTo-ShSingleQuoted $Country)) }
    if ($NoPasswdSudo) { $BoardArgs += "--nopasswd-sudo" }
    if ($NoTrim) { $BoardArgs += "--no-trim" }
    if ($StageOnly) { $BoardArgs += "--stage-only" }
    if ($SkipMcu) { $BoardArgs += "--skip-mcu" }
    if ($SkipKernel) { $BoardArgs += "--skip-kernel" }
    if ($Transport -eq "adb" -and -not $StageOnly) { $BoardArgs += "--force-ap" }
    $BoardArgsStr = $BoardArgs -join " "
}

$Epoch = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
$RemoteCmd = "if [ `"`$(timedatectl show -p NTPSynchronized --value 2>/dev/null || echo no)`" != 'yes' ]; then date -u -s '@$Epoch' >/dev/null 2>&1 || true; fi; /tmp/tactus-install/system/scripts/install-unoq-board.sh $BoardArgsStr"

$RebootNeeded = $false

if ($Transport -eq "adb") {
    if (-not (Get-Command adb -ErrorAction SilentlyContinue)) {
        Show-AdbInstallHint
        exit 1
    }
    $AdbPrefix = @()
    if ($Serial) { $AdbPrefix += @("-s", $Serial) }

    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $state = (& adb @AdbPrefix get-state 2>$null | Out-String).Trim()
    } finally {
        $ErrorActionPreference = $prevEap
    }
    if ($LASTEXITCODE -ne 0 -or $state -ne "device") {
        Fail "No UNO Q detected via USB-C ADB (connect USB-C and check 'adb devices')."
    }

    $NeedSudoPw = $false
    $ArduinoPw = $env:TACTUS_ARDUINO_PASSWORD
    $rc = Invoke-Native { & adb @AdbPrefix shell -T "sudo -n true 2>/dev/null" | Out-Null }
    if ($rc -ne 0) {
        $NeedSudoPw = $true
        $rc = Invoke-Native { & adb @AdbPrefix shell -T "sudo -n -l /usr/local/bin/arduino-passwd >/dev/null 2>&1" | Out-Null }
        if ($rc -eq 0) {
            if ([string]::IsNullOrEmpty($ArduinoPw)) {
                Write-Log "Factory UNO Q detected (no password set for 'arduino')."
                $pw1 = Read-PlainSecret "Set new password for board user 'arduino'"
                if ([string]::IsNullOrEmpty($pw1)) { Fail "Password cannot be empty." }
                $pw2 = Read-PlainSecret "Confirm password for 'arduino'"
                if ($pw1 -ne $pw2) { Fail "Passwords do not match." }
                $ArduinoPw = $pw1
            }
            Write-Log "Setting 'arduino' password via factory /usr/local/bin/arduino-passwd..."
            $rc = Invoke-Native { "$ArduinoPw`n" | & adb @AdbPrefix shell -T "sudo -n /usr/local/bin/arduino-passwd" }
            if ($rc -ne 0) { Fail "factory /usr/local/bin/arduino-passwd failed." }
            $rc = Invoke-Native { "$ArduinoPw`n" | & adb @AdbPrefix shell -T "sudo -S -p '' true" }
            if ($rc -ne 0) { Fail "sudo verification failed." }
        } else {
            if ([string]::IsNullOrEmpty($ArduinoPw)) {
                $ArduinoPw = Read-PlainSecret "Enter existing sudo password for board user 'arduino'"
                if ([string]::IsNullOrEmpty($ArduinoPw)) { Fail "Password cannot be empty." }
            }
            $rc = Invoke-Native { "$ArduinoPw`n" | & adb @AdbPrefix shell -T "sudo -S -p '' true 2>/dev/null" }
            if ($rc -ne 0) { Fail "sudo password rejected for user 'arduino'." }
        }
    }

    try {
        if (-not [string]::IsNullOrEmpty($PubKey) -and (Test-Path $PubKey) -and -not $RollbackAp) {
            $pubBytes = [System.IO.File]::ReadAllBytes($PubKey)
            $pubB64 = [Convert]::ToBase64String($pubBytes)
            Write-Log "Installing SSH public key ($PubKey)..."
            $rc = Invoke-Native { & adb @AdbPrefix shell -T "mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys && key=`$(printf '%s' '$pubB64' | base64 -d | tr -d '\r') && { grep -Fxq `"`$key`" ~/.ssh/authorized_keys || printf '%s\n' `"`$key`" >> ~/.ssh/authorized_keys; }" }
            if ($rc -ne 0) { Fail "Failed to install SSH public key." }
        }

        Write-Log "Uploading release bundle to UNO Q over USB-C ADB..."
        $rc = Invoke-Native { & adb @AdbPrefix shell -T "rm -rf /tmp/tactus-install" }
        if ($rc -ne 0) { Fail "Failed to clean staging directory on board." }
        $rc = Invoke-Native { & adb @AdbPrefix push $ScriptDir /tmp/tactus-install | Out-Null }
        if ($rc -ne 0) { Fail "adb push failed." }
        $rc = Invoke-Native { & adb @AdbPrefix shell -T "sed -i 's/\r$//' /tmp/tactus-install/system/scripts/*.sh && chmod +x /tmp/tactus-install/system/scripts/*.sh" }
        if ($rc -ne 0) { Fail "Failed to prepare scripts on board." }

        $QuotedRemote = ConvertTo-ShSingleQuoted $RemoteCmd
        if (-not $NeedSudoPw) {
            $rc = Invoke-Native { & adb @AdbPrefix shell -T "sudo -n bash -c $QuotedRemote" }
        } else {
            $rc = Invoke-Native { "$ArduinoPw`n" | & adb @AdbPrefix shell -T "sudo -S -p '' bash -c $QuotedRemote" }
        }
        if ($rc -ne 0) { Fail "On-board installer exited with code $rc." }

        $rebootRc = Invoke-Native { & adb @AdbPrefix shell -T "grep -qx 1 /tmp/tactus-install/.reboot-required 2>/dev/null" }
        if ($rebootRc -eq 0) { $RebootNeeded = $true }
    } finally {
        Invoke-Native { & adb @AdbPrefix shell -T "rm -rf /tmp/tactus-install" 2>$null | Out-Null } | Out-Null
    }
} else {
    if (-not (Get-Command ssh -ErrorAction SilentlyContinue)) {
        Fail "'ssh' not found on PATH (enable Windows OpenSSH Client optional feature)."
    }
    if (-not (Get-Command tar -ErrorAction SilentlyContinue)) {
        Fail "'tar' not found on PATH."
    }

    $SshBase = @("-4", "-o", "StrictHostKeyChecking=accept-new")
    if (-not [string]::IsNullOrEmpty($Identity)) {
        $SshBase += @("-i", $Identity, "-o", "IdentitiesOnly=yes")
    }

    if (-not [string]::IsNullOrEmpty($PubKey) -and (Test-Path $PubKey) -and -not $RollbackAp) {
        $batchRc = Invoke-Native { & ssh @SshBase -o BatchMode=yes $Ssh true 2>$null | Out-Null }
        if ($batchRc -ne 0) {
            $pubBytes = [System.IO.File]::ReadAllBytes($PubKey)
            $pubB64 = [Convert]::ToBase64String($pubBytes)
            Write-Log "Installing SSH public key ($PubKey)..."
            $rc = Invoke-Native { & ssh @SshBase $Ssh "mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys && key=`$(printf '%s' '$pubB64' | base64 -d | tr -d '\r') && { grep -Fxq `"`$key`" ~/.ssh/authorized_keys || printf '%s\n' `"`$key`" >> ~/.ssh/authorized_keys; }" }
            if ($rc -ne 0) { Fail "Failed to install SSH public key." }
        }
    }

    try {
        Write-Log "Uploading release bundle to $Ssh..."
        $rc = Invoke-Native { & tar -C $ScriptDir -cf - . | & ssh @SshBase $Ssh "rm -rf /tmp/tactus-install && mkdir -p /tmp/tactus-install && tar -C /tmp/tactus-install -xf - && sed -i 's/\r$//' /tmp/tactus-install/system/scripts/*.sh && chmod +x /tmp/tactus-install/system/scripts/*.sh" }
        if ($rc -ne 0) { Fail "Failed to upload release bundle over SSH." }

        $QuotedRemote = ConvertTo-ShSingleQuoted $RemoteCmd
        $sudoRc = Invoke-Native { & ssh @SshBase $Ssh "sudo -n true" 2>$null | Out-Null }
        if ($sudoRc -eq 0) {
            $rc = Invoke-Native { & ssh @SshBase $Ssh "sudo -n bash -c $QuotedRemote" }
        } else {
            $rc = Invoke-Native { & ssh @SshBase -tt $Ssh "sudo bash -c $QuotedRemote" }
        }
        if ($rc -ne 0) { Fail "On-board installer exited with code $rc." }

        $rebootRc = Invoke-Native { & ssh @SshBase $Ssh "grep -qx 1 /tmp/tactus-install/.reboot-required 2>/dev/null" }
        if ($rebootRc -eq 0) { $RebootNeeded = $true }
    } finally {
        Invoke-Native { & ssh @SshBase $Ssh "rm -rf /tmp/tactus-install" 2>$null | Out-Null } | Out-Null
    }
}

if ($RebootNeeded) {
    $doReboot = $false
    if ($Reboot) { $doReboot = $true }
    elseif (-not $NoReboot -and -not $Yes) {
        $ans = Read-Host "Reboot the UNO Q now to activate the 5 GHz kernel? [Y/n]"
        if ($ans -notmatch "^[Nn]$") { $doReboot = $true }
    }

    if ($doReboot) {
        Write-Log "Rebooting UNO Q..."
        if ($Transport -eq "adb") {
            $sudoRc = Invoke-Native { & adb @AdbPrefix shell -T "sudo -n true 2>/dev/null" | Out-Null }
            if ($sudoRc -eq 0) {
                $rc = Invoke-Native { & adb @AdbPrefix shell -T "sudo -n systemctl reboot" }
            } else {
                $rc = Invoke-Native { "$ArduinoPw`n" | & adb @AdbPrefix shell -T "sudo -S -p '' systemctl reboot" }
            }
        } else {
            $sudoRc = Invoke-Native { & ssh @SshBase $Ssh "sudo -n true" 2>$null | Out-Null }
            if ($sudoRc -eq 0) {
                $rc = Invoke-Native { & ssh @SshBase $Ssh "sudo -n systemctl reboot" }
            } else {
                $rc = Invoke-Native { & ssh @SshBase -tt $Ssh "sudo systemctl reboot" }
            }
        }
        if ($rc -eq 0 -or $rc -eq 255) {
            Write-Log "Board is rebooting. Connect to WiFi SSID 'Tactus' and open http://$Hostname.local or http://10.42.0.1"
        } else {
            Fail "Reboot command failed (exit $rc); run 'sudo reboot' on the board manually."
        }
    } else {
        Write-Log "Skipping reboot. Run 'sudo reboot' on the board later to activate the 5 GHz kernel."
    }
}
