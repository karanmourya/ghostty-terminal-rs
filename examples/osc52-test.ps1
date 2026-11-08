# OSC 52 smoke test: ask the terminal to put text on the clipboard,
# then read the Windows clipboard back and report.
$esc = [char]27
$bel = [char]7
$text = "OSC52-hello-from-terminal"
$bytes = [Text.Encoding]::UTF8.GetBytes($text)
$b64 = [Convert]::ToBase64String($bytes)
[Console]::Write("$esc]52;c;$b64$bel")
Start-Sleep -Milliseconds 1200
$clip = Get-Clipboard -Raw
if ($clip -eq $text) { Write-Host "OSC52-OK clipboard round-trip" } else { Write-Host "OSC52-FAIL got [$clip]" }

# Query path: set the clipboard ourselves, query it back through OSC 52,
# and dump the reply with cat -v style formatting via od.
Set-Clipboard -Value "query-me"
Start-Sleep -Milliseconds 300
[Console]::Write("$esc]52;c;?$bel")
Start-Sleep -Milliseconds 1200
Write-Host "query sent (reply is in the raw stream above if echoed)"
