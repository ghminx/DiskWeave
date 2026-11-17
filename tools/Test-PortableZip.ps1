param([Parameter(Mandatory)][string]$Zip, [switch]$Gui)
$ErrorActionPreference = 'Stop'
$path = (Resolve-Path -LiteralPath $Zip).Path
$repo = Split-Path $PSScriptRoot
$archive = [IO.Compression.ZipFile]::OpenRead($path)
try {
    $names = @($archive.Entries | Where-Object Length -GT 0 | ForEach-Object FullName)
    foreach ($name in $names) {
        if ($name.Contains('..') -or $name.Contains(':') -or $name.StartsWith('/')) { throw "잘못된 ZIP 경로: $name" }
        if ($name -notin @('DiskWeave.exe','README.md','THIRD_PARTY_NOTICES.md') -and !$name.StartsWith('licenses/')) { throw "허용하지 않은 ZIP 내용: $name" }
    }
    foreach ($name in @('DiskWeave.exe','README.md','THIRD_PARTY_NOTICES.md','licenses/Pretendard/OFL.txt','licenses/Rust/COPYRIGHT-library.html')) {
        if ($name -notin $names) { throw "필수 ZIP 내용 누락: $name" }
    }
    $reader = [IO.StreamReader]::new($archive.GetEntry('THIRD_PARTY_NOTICES.md').Open())
    try { $notices = $reader.ReadToEnd() } finally { $reader.Dispose() }
    $references = [regex]::Matches($notices,'License texts: licenses/([^/]+)/')
    if ($references.Count -eq 0) { throw '크레이트 라이선스 목록 누락' }
    foreach ($reference in $references) {
        $prefix = 'licenses/'+$reference.Groups[1].Value+'/'
        if (!($names | Where-Object { $_.StartsWith($prefix) })) { throw "고지 원문 누락: $prefix" }
    }
    foreach ($entry in $archive.Entries | Where-Object { $_.Length -gt 0 -and $_.FullName -ne 'DiskWeave.exe' }) {
        $reader = [IO.StreamReader]::new($entry.Open())
        try { $text = $reader.ReadToEnd() } finally { $reader.Dispose() }
        foreach ($private in @($repo,$repo.Replace('\','/'),$env:USERPROFILE,$env:USERPROFILE.Replace('\','/'))) {
            if ($private.Length -ge 3 -and $text.Contains($private,[StringComparison]::OrdinalIgnoreCase)) { throw "개인 경로/정보 포함: $($entry.FullName)" }
        }
    }
} finally { $archive.Dispose() }
$extract = Join-Path ([IO.Path]::GetTempPath()) ('DiskWeave 압축 해제 시험 '+[guid]::NewGuid().ToString('N'))
[IO.Compression.ZipFile]::ExtractToDirectory($path,$extract)
& (Join-Path $PSScriptRoot 'Test-Portable.ps1') -Executable (Join-Path $extract 'DiskWeave.exe') -Gui:$Gui
Write-Output ('PASS: ZIP whitelist/privacy/licenses; files='+$names.Count)
