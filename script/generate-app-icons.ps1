param(
    [Parameter(Mandatory = $true)]
    [string]$SourcePng,
    [string]$ProjectRoot = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

function New-IconPngBytes {
    param(
        [System.Drawing.Bitmap]$Source,
        [int]$Size,
        [int]$Padding
    )

    $result = [System.Drawing.Bitmap]::new(
        $Size,
        $Size,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb
    )
    $graphics = [System.Drawing.Graphics]::FromImage($result)
    try {
        $graphics.Clear([System.Drawing.Color]::Transparent)
        $graphics.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
        $graphics.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
        $graphics.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
        $graphics.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
        $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality

        $side = $Size - (2 * $Padding)
        $destination = [System.Drawing.Rectangle]::new($Padding, $Padding, $side, $side)
        $sourceRectangle = [System.Drawing.Rectangle]::new(0, 0, $Source.Width, $Source.Height)
        $graphics.DrawImage(
            $Source,
            $destination,
            $sourceRectangle,
            [System.Drawing.GraphicsUnit]::Pixel
        )
    }
    finally {
        $graphics.Dispose()
    }

    $stream = [System.IO.MemoryStream]::new()
    try {
        $result.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
        return ,$stream.ToArray()
    }
    finally {
        $stream.Dispose()
        $result.Dispose()
    }
}

function New-CleanSourceBitmap {
    param([System.Drawing.Bitmap]$Source)

    $clean = [System.Drawing.Bitmap]::new(
        $Source.Width,
        $Source.Height,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb
    )
    $graphics = [System.Drawing.Graphics]::FromImage($clean)
    $transparent = [System.Drawing.SolidBrush]::new([System.Drawing.Color]::Transparent)
    try {
        $graphics.CompositingMode = [System.Drawing.Drawing2D.CompositingMode]::SourceCopy
        $graphics.DrawImageUnscaled($Source, 0, 0)
        # Image generators can leave nearly transparent flecks at the canvas edge.
        # The artwork has a deliberate safe area, so clearing this narrow border
        # removes those artifacts without touching the rounded-square icon.
        $border = [math]::Max(1, [math]::Floor($Source.Width * 0.032))
        $graphics.FillRectangle($transparent, 0, 0, $Source.Width, $border)
        $graphics.FillRectangle($transparent, 0, $Source.Height - $border, $Source.Width, $border)
        $graphics.FillRectangle($transparent, 0, 0, $border, $Source.Height)
        $graphics.FillRectangle($transparent, $Source.Width - $border, 0, $border, $Source.Height)
    }
    finally {
        $transparent.Dispose()
        $graphics.Dispose()
    }
    return $clean
}

function Write-BigEndianUInt32 {
    param([System.IO.BinaryWriter]$Writer, [uint32]$Value)
    $Writer.Write([byte](($Value -shr 24) -band 0xff))
    $Writer.Write([byte](($Value -shr 16) -band 0xff))
    $Writer.Write([byte](($Value -shr 8) -band 0xff))
    $Writer.Write([byte]($Value -band 0xff))
}

$resolvedSource = (Resolve-Path -LiteralPath $SourcePng).Path
$resolvedRoot = (Resolve-Path -LiteralPath $ProjectRoot).Path
$source = [System.Drawing.Bitmap]::FromFile($resolvedSource)
try {
    if ($source.Width -ne $source.Height) {
        throw "Source image must be square: ${resolvedSource}"
    }

    $cleanSource = New-CleanSourceBitmap -Source $source
    try {
    $masterBytes = New-IconPngBytes -Source $cleanSource -Size 1024 -Padding 0
    [System.IO.File]::WriteAllBytes(
        (Join-Path $resolvedRoot "resources/navop-icon.png"),
        $masterBytes
    )

    $icoFrames = foreach ($size in @(16, 24, 32, 48, 64, 128, 256)) {
        [pscustomobject]@{
            Size = $size
            Bytes = (New-IconPngBytes -Source $cleanSource -Size $size -Padding 0)
        }
    }
    $icoPath = Join-Path $resolvedRoot "resources/windows/navop.ico"
    $icoStream = [System.IO.File]::Create($icoPath)
    $icoWriter = [System.IO.BinaryWriter]::new($icoStream)
    try {
        $icoWriter.Write([uint16]0)
        $icoWriter.Write([uint16]1)
        $icoWriter.Write([uint16]$icoFrames.Count)
        $offset = 6 + (16 * $icoFrames.Count)
        foreach ($frame in $icoFrames) {
            $dimension = if ($frame.Size -eq 256) { 0 } else { $frame.Size }
            $icoWriter.Write([byte]$dimension)
            $icoWriter.Write([byte]$dimension)
            $icoWriter.Write([byte]0)
            $icoWriter.Write([byte]0)
            $icoWriter.Write([uint16]1)
            $icoWriter.Write([uint16]32)
            $icoWriter.Write([uint32]$frame.Bytes.Length)
            $icoWriter.Write([uint32]$offset)
            $offset += $frame.Bytes.Length
        }
        foreach ($frame in $icoFrames) {
            $icoWriter.Write($frame.Bytes)
        }
    }
    finally {
        $icoWriter.Dispose()
        $icoStream.Dispose()
    }

    $icnsFrames = @(
        @{ Type = "icp4"; Size = 16 },
        @{ Type = "icp5"; Size = 32 },
        @{ Type = "icp6"; Size = 64 },
        @{ Type = "ic07"; Size = 128 },
        @{ Type = "ic08"; Size = 256 },
        @{ Type = "ic09"; Size = 512 },
        @{ Type = "ic10"; Size = 1024 }
    ) | ForEach-Object {
        [pscustomobject]@{
            Type = $_.Type
            Bytes = (New-IconPngBytes -Source $cleanSource -Size $_.Size -Padding ([math]::Round($_.Size * 0.078125)))
        }
    }
    $icnsPath = Join-Path $resolvedRoot "resources/macos/Navop.icns"
    $icnsStream = [System.IO.File]::Create($icnsPath)
    $icnsWriter = [System.IO.BinaryWriter]::new($icnsStream)
    try {
        $totalLength = 8 + (($icnsFrames | ForEach-Object { 8 + $_.Bytes.Length } | Measure-Object -Sum).Sum)
        $icnsWriter.Write([System.Text.Encoding]::ASCII.GetBytes("icns"))
        Write-BigEndianUInt32 -Writer $icnsWriter -Value $totalLength
        foreach ($frame in $icnsFrames) {
            $icnsWriter.Write([System.Text.Encoding]::ASCII.GetBytes($frame.Type))
            Write-BigEndianUInt32 -Writer $icnsWriter -Value (8 + $frame.Bytes.Length)
            $icnsWriter.Write($frame.Bytes)
        }
    }
    finally {
        $icnsWriter.Dispose()
        $icnsStream.Dispose()
    }
    }
    finally {
        $cleanSource.Dispose()
    }
}
finally {
    $source.Dispose()
}

Write-Host "Generated resources/navop-icon.png, resources/windows/navop.ico, and resources/macos/Navop.icns"
