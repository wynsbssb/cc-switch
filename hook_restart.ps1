$ErrorActionPreference = 'Stop'
$exe = 'C:\Program Files\WindowsApps\OpenAI.Codex_26.803.10989.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe'
Write-Output '== stopping ChatGPT/codex =='
Get-Process ChatGPT -ErrorAction SilentlyContinue | Stop-Process -Force
Get-Process codex -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 1000
Write-Output '== launching with CDP port 9333 =='
Start-Process -FilePath $exe -ArgumentList '--remote-debugging-port=9333','--remote-allow-origins=*'
Start-Sleep -Seconds 4
Write-Output '== running cdp inject =='
$env:CDP_PORT = '9333'
node 'D:\cc-switch-main\cc-switch-main\cdp_inject.cjs'
Write-Output "== exit code $LASTEXITCODE =="
