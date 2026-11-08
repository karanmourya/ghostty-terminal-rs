# Visual smoke test for the renderer (UTF-8 with BOM so PowerShell 5.1 reads it correctly).
$OutputEncoding = [Console]::OutputEncoding = [Text.Encoding]::UTF8
# Visual smoke test: prints ANSI colors, styles, wide glyphs, and box drawing.
# Run the terminal with:
#   $env:GHOSTTY_WIN_SHELL = "powershell.exe -NoLogo -NoExit -File C:\Ghostty-windows\demo.ps1"
#   cargo run

$e = [char]27

Write-Host ""
Write-Host "$e[1mGhostty (Rust) renderer smoke test$e[0m"
Write-Host ""

# 16 ANSI colors
$line = ""
for ($i = 30; $i -le 37; $i++) { $line += "$e[${i}m  $i  $e[0m " }
Write-Host $line
$line = ""
for ($i = 90; $i -le 97; $i++) { $line += "$e[${i}m $i $e[0m " }
Write-Host $line

# 256-color gradient
Write-Host ""
$line = ""
foreach ($i in 17, 33, 51, 70, 100, 129, 165, 200, 214, 227) { $line += "$e[48;5;${i}m   $e[0m" }
Write-Host $line

# truecolor
Write-Host ""
$line = ""
for ($r = 0; $r -lt 6; $r++) {
    $v = $r * 51
    $line += "$e[48;2;${v};80;$((255 - $v))m   $e[0m"
}
Write-Host $line

# styles
Write-Host ""
Write-Host "$e[1mbold$e[0m  $e[4munderline$e[0m  $e[7minverse$e[0m  $e[1;4;31mbold+underline+red$e[0m"

# wide glyphs and emoji
Write-Host ""
Write-Host "Wide: 日本語のテキスト 中文字符 한국어"
Write-Host "Mixed: ASCII-日本語-ASCII"

# box drawing + cursor motion
Write-Host ""
Write-Host "┌──────────────────────────┐"
Write-Host "│  box drawing + cursor    │"
Write-Host "└──────────────────────────┘"
Write-Host ""

Write-Host -NoNewline "$e[32mReady.$e[0m "
