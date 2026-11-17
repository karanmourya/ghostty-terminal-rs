# Kitty graphics smoke test: emit a transmitted PNG and place it on screen.
Add-Type -AssemblyName System.Drawing

$bmp = New-Object System.Drawing.Bitmap 64, 64
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.Clear([System.Drawing.Color]::FromArgb(20, 20, 30))
$brush = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(220, 80, 120))
$g.FillEllipse($brush, 4, 4, 56, 56)
$brush2 = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(120, 200, 255))
$g.FillEllipse($brush2, 20, 20, 24, 24)
$g.Dispose(); $brush.Dispose(); $brush2.Dispose()

$ms = New-Object System.IO.MemoryStream
$bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
$b64 = [Convert]::ToBase64String($ms.ToArray())
$bmp.Dispose(); $ms.Dispose()

$enc = [Text.Encoding]::ASCII
$out = [Console]::OpenStandardOutput()
$seg1 = "$([char]27)" + "PGa=T,f=100,i=1,d=$b64$([char]27)\"
$seg2 = "$([char]27)" + "PGa=p,i=1,c=12,r=8,C=1,R=1$([char]27)\"
$bytes = $enc.GetBytes($seg1 + $seg2 + "kitty image placed`r`n")
$out.Write($bytes, 0, $bytes.Length)
$out.Flush()
