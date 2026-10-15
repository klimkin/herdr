[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $RunnerArguments
)
$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') {
    throw 'Run this script on native Windows, outside WSL.'
}
if (Get-Command python -ErrorAction SilentlyContinue) {
    & python "$PSScriptRoot/remeasure.py" @RunnerArguments
} elseif (Get-Command py -ErrorAction SilentlyContinue) {
    & py -3 "$PSScriptRoot/remeasure.py" @RunnerArguments
} else {
    throw 'Install Python 3.10 or newer.'
}
exit $LASTEXITCODE
