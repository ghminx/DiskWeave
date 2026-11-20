param([Parameter(Mandatory)][string]$Zip)
$ErrorActionPreference = 'Stop'
$package = (Resolve-Path -LiteralPath $Zip).Path
$pwsh = Join-Path $PSHOME 'pwsh.exe'
$token = [guid]::NewGuid().ToString('N')
$fixture = Join-Path ([IO.Path]::GetTempPath()) "diskweave-portable-guards-$token"
$exe = Join-Path $fixture 'DiskWeave.exe'
$variant = Join-Path $fixture 'variant.zip'
$oldCargoHome = $env:CARGO_HOME

function Assert-Guard([string[]]$Arguments, [string]$Failure) {
    $output = & $pwsh -NoProfile -File @Arguments 2>&1
    $code = $LASTEXITCODE
    if ($Failure) {
        if ($code -ne 1 -or !($output | Out-String).Contains($Failure)) {
            throw "다른 검사 결과 또는 종료 코드: $code ($output)"
        }
    } elseif ($code -ne 0) {
        throw "정상 입력의 검사 실패: $code ($output)"
    }
}

[void][IO.Directory]::CreateDirectory($fixture)
try {
    $archive = [IO.Compression.ZipFile]::OpenRead($package)
    try {
        $entry = $archive.GetEntry('DiskWeave.exe')
        [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $exe)
    } finally { $archive.Dispose() }
    $clean = [IO.File]::ReadAllBytes($exe)
    $env:CARGO_HOME = "C:\DiskWeave-Guard-Only\$token"
    foreach ($encoding in @([Text.Encoding]::UTF8, [Text.Encoding]::Unicode)) {
        $marker = $encoding.GetBytes($env:CARGO_HOME.Replace('\', '/'))
        [IO.File]::WriteAllBytes($exe, $clean)
        $stream = [IO.File]::Open($exe, [IO.FileMode]::Append)
        try { $stream.Write($marker, 0, $marker.Length) } finally { $stream.Dispose() }
        Assert-Guard @((Join-Path $PSScriptRoot 'Test-Portable.ps1'), '-Executable', $exe) '실행 파일에 개발 환경 경로 포함'
    }
    'PASS: EXE 개발 경로 UTF-8·UTF-16 검사'
    $env:CARGO_HOME = $oldCargoHome
    foreach ($case in @(@('unlisted.txt', '허용하지 않은 ZIP 내용: unlisted.txt'), @('licenses/test-directory/'), @('NOTICE', '필수 ZIP 내용 누락: NOTICE'))) {
        Copy-Item -LiteralPath $package -Destination $variant -Force
        $archive = [IO.Compression.ZipFile]::Open($variant, [IO.Compression.ZipArchiveMode]::Update)
        try {
            if ($case[0] -eq 'NOTICE') { $archive.GetEntry('NOTICE').Delete() }
            [void]$archive.CreateEntry($case[0])
        } finally { $archive.Dispose() }
        Assert-Guard @((Join-Path $PSScriptRoot 'Test-PortableZip.ps1'), '-Zip', $variant) $case[1]
    }
    'PASS: 빈 미등록 파일·정상 폴더·빈 NOTICE 검사'
} finally {
    $env:CARGO_HOME = $oldCargoHome
    [IO.File]::Delete($exe)
    [IO.File]::Delete($variant)
    [IO.Directory]::Delete($fixture)
}
