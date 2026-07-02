# scripts/copilot-login.ps1
#
# Authenticate to GitHub Copilot via the GitHub OAuth device flow and persist
# credentials in the exact format AgentShim expects. This is a self-contained
# PowerShell port of `agent-shim copilot login` — it does NOT require the
# compiled binary, so it can be run to seed credentials before/without a build.
#
# The credential file it writes is byte-compatible with what the Rust CLI reads:
#   - default location : %APPDATA%\agent-shim\copilot-credentials.json
#                        (matches dirs::config_dir() on Windows)
#   - JSON shape        : { "github_oauth_token": "...", "created_at_unix": <i64> }
#     (matches providers::github_copilot::credential_store::StoredCredentials)
#
# The OAuth client id / scope are mirrored from
#   crates/providers/src/github_copilot/headers.rs
# Keep them in sync if that file changes.
#
# Usage:
#   pwsh -File scripts\copilot-login.ps1
#   pwsh -File scripts\copilot-login.ps1 -OpenBrowser
#   pwsh -File scripts\copilot-login.ps1 -CredentialPath C:\path\to\creds.json

[CmdletBinding()]
param(
    # Override the credential output path. Defaults to the AgentShim location.
    [string]$CredentialPath,

    # Automatically open the verification URL in the default browser.
    [switch]$OpenBrowser
)

$ErrorActionPreference = 'Stop'
$ProgressPreference     = 'SilentlyContinue'

# GitHub requires TLS 1.2; Windows PowerShell 5.1 may default to older protocols.
try {
    [Net.ServicePointManager]::SecurityProtocol = `
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {
    # PS 7 manages this itself; ignore if the enum/API is unavailable.
}

# --- Constants mirrored from headers.rs -----------------------------------
$ClientId = 'Iv1.b507a08c87ecfe98'   # COPILOT_OAUTH_CLIENT_ID
$Scope    = 'read:user'              # COPILOT_OAUTH_SCOPE

$DeviceCodeUri  = 'https://github.com/login/device/code'
$AccessTokenUri = 'https://github.com/login/oauth/access_token'
$GrantType      = 'urn:ietf:params:oauth:grant-type:device_code'

function Write-Step {
    param([string]$Message)
    Write-Host ""
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Write-Info {
    param([string]$Message)
    Write-Host "    $Message" -ForegroundColor DarkGray
}

# POST an x-www-form-urlencoded body and return the parsed JSON object.
# Handles non-2xx responses (GitHub returns error JSON on some failures) by
# extracting and parsing the response body across PS 5.1 and PS 7.
function Invoke-FormPost {
    param(
        [string]$Uri,
        [hashtable]$Form
    )

    try {
        return Invoke-RestMethod -Method Post -Uri $Uri `
            -Headers @{ Accept = 'application/json' } `
            -Body $Form -TimeoutSec 30
    } catch {
        $body = $null

        # PowerShell 7: the response body is surfaced here.
        if ($_.ErrorDetails -and $_.ErrorDetails.Message) {
            $body = $_.ErrorDetails.Message
        }
        # Windows PowerShell 5.1: read the body off the response stream.
        elseif ($_.Exception.Response) {
            try {
                $stream = $_.Exception.Response.GetResponseStream()
                $reader = New-Object System.IO.StreamReader($stream)
                $body   = $reader.ReadToEnd()
                $reader.Dispose()
            } catch { }
        }

        if ($body) {
            try { return $body | ConvertFrom-Json } catch { }
        }

        throw "HTTP request to $Uri failed: $($_.Exception.Message)"
    }
}

# --- Resolve credential path ----------------------------------------------
if (-not $CredentialPath) {
    $appData = $env:APPDATA
    if (-not $appData) {
        throw "Could not determine %APPDATA%; pass -CredentialPath explicitly."
    }
    $CredentialPath = Join-Path $appData 'agent-shim\copilot-credentials.json'
}

Write-Step "Logging in to GitHub Copilot"
Write-Info "Credentials will be saved to: $CredentialPath"

# --- Step 1: request device + user codes ----------------------------------
$device = Invoke-FormPost -Uri $DeviceCodeUri -Form @{
    client_id = $ClientId
    scope     = $Scope
}

$deviceCode      = $device.device_code
$userCode        = if ($device.user_code)        { $device.user_code }        else { '???' }
$verificationUri = if ($device.verification_uri) { $device.verification_uri } else { 'https://github.com/login/device' }
$intervalSecs    = if ($device.interval)         { [int]$device.interval }    else { 5 }
$expiresIn       = if ($device.expires_in)       { [int]$device.expires_in }  else { 900 }

if (-not $deviceCode) {
    throw "Device code response did not contain a device_code."
}

# --- Step 2: display instructions -----------------------------------------
Write-Step "Authorize this device"
Write-Host "    Open the following URL and enter the code below:"
Write-Host "      URL:  $verificationUri" -ForegroundColor Yellow
Write-Host "      Code: $userCode"        -ForegroundColor Yellow

# Convenience: copy the code to the clipboard so it can be pasted directly.
try { Set-Clipboard -Value $userCode; Write-Info "(code copied to clipboard)" } catch { }

if ($OpenBrowser) {
    try { Start-Process $verificationUri | Out-Null } catch {
        Write-Info "Could not open a browser automatically; open the URL manually."
    }
}

Write-Host ""
Write-Host "    Waiting for authorization…" -ForegroundColor DarkGray

# --- Step 3: poll until approved or denied --------------------------------
$deadline = (Get-Date).AddSeconds($expiresIn)

while ($true) {
    Start-Sleep -Seconds $intervalSecs

    if ((Get-Date) -gt $deadline) {
        throw "Device code expired before authorization completed. Re-run to try again."
    }

    $poll = Invoke-FormPost -Uri $AccessTokenUri -Form @{
        client_id   = $ClientId
        device_code = $deviceCode
        grant_type  = $GrantType
    }

    if ($poll.access_token) {
        $token     = $poll.access_token
        $createdAt = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()

        # Match StoredCredentials field names exactly (snake_case).
        $creds = [ordered]@{
            github_oauth_token = $token
            created_at_unix    = $createdAt
        }
        $json = $creds | ConvertTo-Json

        $dir = Split-Path -Parent $CredentialPath
        if ($dir -and -not (Test-Path $dir)) {
            New-Item -ItemType Directory -Path $dir -Force | Out-Null
        }

        # Write UTF-8 WITHOUT BOM — serde_json does not skip a leading BOM.
        $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
        [System.IO.File]::WriteAllText($CredentialPath, $json, $utf8NoBom)

        Write-Step "Success"
        Write-Host "    Successfully authenticated. Credentials saved to:" -ForegroundColor Green
        Write-Host "      $CredentialPath" -ForegroundColor Green
        exit 0
    }

    if ($poll.error) {
        switch ($poll.error) {
            'authorization_pending' { continue }
            'slow_down' {
                # RFC 8628: back off. Honor a new interval if provided, else +5s.
                if ($poll.interval) { $intervalSecs = [int]$poll.interval } else { $intervalSecs += 5 }
                continue
            }
            'access_denied' {
                Write-Step "Declined"
                Write-Host "    Authorization was declined." -ForegroundColor Red
                exit 1
            }
            default {
                throw "Device flow error: $($poll.error) $($poll.error_description)"
            }
        }
    }
}
