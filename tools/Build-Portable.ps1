param([switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot
Push-Location $repo
try {
    if (!$SkipBuild) {
        $previous = $env:CARGO_ENCODED_RUSTFLAGS
        try {
            $separator = [string][char]31
            $flags = @()
            if ($previous) {
                $flags += $previous.Split([char]31)
            } elseif ($env:RUSTFLAGS) {
                $flags += $env:RUSTFLAGS.Trim() -split '\s+'
            }
            $flags += '-C', 'target-feature=+crt-static'
            $cargoHome = if ($env:CARGO_HOME) { [IO.Path]::GetFullPath($env:CARGO_HOME) } else { Join-Path $env:USERPROFILE '.cargo' }
            foreach ($pair in @(@($env:USERPROFILE, '/build-user'), @($cargoHome, '/cargo'), @($repo, '/diskweave'))) {
                $root = $pair[0].TrimEnd('\', '/')
                foreach ($prefix in @($root, $root.Replace('\', '/')) | Select-Object -Unique) {
                    $flags += "--remap-path-prefix=$prefix=$($pair[1])"
                }
            }
            $env:CARGO_ENCODED_RUSTFLAGS = $flags -join $separator
            cargo build --locked --release --target x86_64-pc-windows-msvc --target-dir target/portable-build
            if ($LASTEXITCODE -ne 0) { throw 'Portable Release 빌드 실패' }
        } finally { $env:CARGO_ENCODED_RUSTFLAGS = $previous }
    }
    $metadataJson = cargo metadata --locked --filter-platform x86_64-pc-windows-msvc --format-version 1
    if ($LASTEXITCODE -ne 0) { throw '의존성 메타데이터 확인 실패' }
    $metadata = $metadataJson | ConvertFrom-Json
    $tree = cargo tree --locked --target x86_64-pc-windows-msvc --edges normal --prefix none --format '{p}'
    if ($LASTEXITCODE -ne 0) { throw 'Windows 의존성 목록 확인 실패' }
    $used = @{}
    foreach ($line in $tree) { if ($line -match '^(\S+) v([^ ]+)') { $used[$Matches[1]+'@'+$Matches[2]]=$true } }
    $product = $metadata.packages | Where-Object name -EQ diskweave
    $version = $product.version
    $dist = Join-Path $repo 'target/dist'
    $stage = Join-Path $dist ('staging-'+[guid]::NewGuid().ToString('N'))
    [void][IO.Directory]::CreateDirectory($stage)
    Copy-Item -LiteralPath 'target/portable-build/x86_64-pc-windows-msvc/release/diskweave.exe' -Destination (Join-Path $stage 'DiskWeave.exe')
    & (Join-Path $PSScriptRoot 'Test-Portable.ps1') -Executable (Join-Path $stage 'DiskWeave.exe')
    Copy-Item -LiteralPath 'docs/portable-readme.md' -Destination (Join-Path $stage 'README.md')
    Copy-Item -LiteralPath 'LICENSE','NOTICE' -Destination $stage
    $notices = [Collections.Generic.List[string]]::new()
    $notices.Add("# DiskWeave $version — third-party notices")
    $notices.Add("DiskWeave is licensed under $($product.license) (see LICENSE and NOTICE). The components below retain their respective license terms.")
    foreach ($package in ($metadata.packages | Sort-Object name,version)) {
        $id = $package.name+'@'+$package.version
        if ($package.name -eq 'diskweave' -or !$used.ContainsKey($id)) { continue }
        if (!$package.license) { throw "라이선스 선언 누락: $id" }
        $directory = Split-Path $package.manifest_path
        $files = @(Get-ChildItem -LiteralPath $directory -File | Where-Object Name -Match '^(LICENSE|LICENCE|COPYING|NOTICE|COPYRIGHT)')
        $vendored = Join-Path $repo "resources/licenses/$id"
        if (Test-Path -LiteralPath $vendored) { $files += @(Get-ChildItem -LiteralPath $vendored -File | Where-Object Name -NotMatch '^SOURCES') }
        if (!$files.Count) { throw "라이선스 원문 누락: $id" }
        $destination = Join-Path $stage "licenses/$id"
        [void][IO.Directory]::CreateDirectory($destination)
        foreach ($file in $files) { Copy-Item -LiteralPath $file.FullName -Destination $destination }
        if ($package.name -eq 'epaint_default_fonts') {
            Get-ChildItem -LiteralPath (Join-Path $directory 'fonts') -Filter '*.txt' | Copy-Item -Destination $destination
        }
        $notices.Add("`n## $id`nLicense: $($package.license)`nSource: $($package.repository)`nLicense texts: licenses/$id/")
    }
    $fonts = Join-Path $stage 'licenses/Pretendard'
    [void][IO.Directory]::CreateDirectory($fonts)
    Copy-Item -LiteralPath 'assets/fonts/OFL.txt' -Destination $fonts
    $notices.Add("`n## Pretendard`nLicense: SIL OFL 1.1`nSource: https://github.com/orioncactus/pretendard`nLicense text: licenses/Pretendard/OFL.txt")
    $rust = Join-Path $stage 'licenses/Rust'
    [void][IO.Directory]::CreateDirectory($rust)
    $sysroot = rustc --print sysroot
    if ($LASTEXITCODE -ne 0) { throw 'Rust 고지 경로 확인 실패' }
    foreach ($name in @('COPYRIGHT-library.html')) {
        Copy-Item -LiteralPath (Join-Path $sysroot "share/doc/rust/$name") -Destination $rust
    }
    Copy-Item -LiteralPath (Join-Path $sysroot 'share/doc/rust/licenses') -Destination $rust -Recurse
    $notices.Add("`n## Rust standard library`nLicense: MIT OR Apache-2.0; bundled notices: licenses/Rust/")
    Copy-Item -LiteralPath 'resources/licenses/SOURCES.txt' -Destination (Join-Path $stage 'licenses/SOURCES.txt')
    [IO.File]::WriteAllLines((Join-Path $stage 'THIRD_PARTY_NOTICES.md'), $notices)
    # 지정한 구성 파일만 패키징하고 빌드 경로·사용자 자료 유입 차단
    foreach ($file in Get-ChildItem -LiteralPath $stage -Recurse -File) {
        $relative = [IO.Path]::GetRelativePath($stage,$file.FullName).Replace('\','/')
        if ($relative -notin @('DiskWeave.exe','README.md','LICENSE','NOTICE','THIRD_PARTY_NOTICES.md') -and !$relative.StartsWith('licenses/')) { throw "허용하지 않은 포함 파일: $relative" }
    }
    $zip = Join-Path $dist "DiskWeave-$version-windows-x64-preview.zip"
    foreach ($file in Get-ChildItem -LiteralPath $stage -Recurse -File) { $file.LastWriteTimeUtc = [DateTime]::new(2000,1,1,0,0,0,[DateTimeKind]::Utc) }
    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -Force
    Write-Output $zip
} finally { Pop-Location }
