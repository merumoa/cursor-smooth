param([string]$Path)
Add-Type -AssemblyName System.Drawing
$arrow = New-Object 'System.Drawing.Point[]' 7
$arrow[0] = New-Object System.Drawing.Point(9, 4)
$arrow[1] = New-Object System.Drawing.Point(9, 24)
$arrow[2] = New-Object System.Drawing.Point(14, 19)
$arrow[3] = New-Object System.Drawing.Point(17, 27)
$arrow[4] = New-Object System.Drawing.Point(21, 25)
$arrow[5] = New-Object System.Drawing.Point(18, 17)
$arrow[6] = New-Object System.Drawing.Point(24, 17)
$bitmap = New-Object System.Drawing.Bitmap 32, 32
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
$graphics.Clear([System.Drawing.Color]::Transparent)
$white = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::White)
$graphics.FillPolygon($white, $arrow)
$pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(30, 30, 30)), 2.0
$graphics.DrawPolygon($pen, $arrow)
$handle = $bitmap.GetHicon()
$icon = [System.Drawing.Icon]::FromHandle($handle)
$stream = [System.IO.File]::Create($Path)
$icon.Save($stream)
$stream.Close()
$icon.Dispose()
$bitmap.Dispose()
$graphics.Dispose()
Write-Output "icon: $Path"
