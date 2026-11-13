# Prints a couple of long lines so they soft-wrap across many rows.
Write-Host ("LINE-01 " + ("x" * 300))
Write-Host ("LINE-02 " + ("y" * 300))
Write-Host "resize-marker"
Start-Sleep -Seconds 30
