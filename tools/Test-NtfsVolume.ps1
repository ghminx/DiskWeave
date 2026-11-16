param([string]$Runner,[ValidateRange(0,10000)][int]$FileCount=10000,[switch]$Usn,[switch]$FileManagement,[switch]$WaitForClient,[switch]$Layout,[switch]$Mft,[switch]$ProductMft)
$ErrorActionPreference='Stop'
$OutputEncoding=[Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)
$workspace=[IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$ownedRoot=[IO.Path]::GetFullPath((Join-Path $workspace 'target/ntfs-validation'))
if(-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {throw '새 시험용 VHD를 생성·연결하려면 관리자 권한이 필요함. 기존 볼륨은 변경하지 않음.'}
if(-not $Runner){$Runner=Join-Path $workspace 'target/release/diskweave-ntfs-validation.exe'}
$Runner=(Resolve-Path -LiteralPath $Runner).Path
if(-not $Runner.StartsWith($workspace+[IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase)){throw '프로젝트 내부 검증 실행 파일만 허용함'}
$token=[Guid]::NewGuid().ToString('N')
$runDir=Join-Path $ownedRoot $token
[void][IO.Directory]::CreateDirectory($runDir)
$image=[IO.Path]::GetFullPath((Join-Path $runDir 'fixture.vhdx'))
if(-not $image.StartsWith($ownedRoot+[IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase) -or (Test-Path -LiteralPath $image)){throw '새 시험 이미지 경로 확인 실패'}
$script=Join-Path $runDir 'create.txt'
[IO.File]::WriteAllText($script,"create vdisk file=`"$image`" maximum=256 type=expandable`r`nexit`r`n")
$mounted=$false
try {
    & diskpart.exe /s $script | Out-File (Join-Path $runDir 'diskpart.log')
    if(-not (Test-Path -LiteralPath $image)){throw '시험 이미지 생성 실패'}
    $attachment=Mount-DiskImage -ImagePath $image -StorageType VHDX -PassThru
    $mounted=$true
    $verified=Get-DiskImage -ImagePath $image
    if(-not $verified.Attached -or [IO.Path]::GetFullPath($verified.ImagePath) -ne $image){throw '연결한 이미지 소유 확인 실패'}
    $disk=$verified | Get-Disk
    if($disk.IsBoot -or $disk.IsSystem -or $disk.PartitionStyle -ne 'RAW' -or $disk.Size -gt 300MB -or @($disk|Get-Partition -ErrorAction SilentlyContinue).Count -ne 0){throw '새 빈 시험 디스크 조건 확인 실패'}
    # backing image와 RAW 상태를 확인한 객체에만 초기화·포맷 적용
    $disk | Initialize-Disk -PartitionStyle GPT
    $partition=Get-DiskImage -ImagePath $image | Get-Disk | New-Partition -UseMaximumSize -AssignDriveLetter
    $volume=$partition | Format-Volume -FileSystem NTFS -NewFileSystemLabel ('DWTEST-'+$token.Substring(0,8)) -Confirm:$false -Force
    $root=([string]$volume.DriveLetter)+':\'
    if($root.Substring(0,2) -eq $env:SystemDrive){throw '시스템 드라이브는 시험 대상에서 제외함'}
    $resolved=Get-DiskImage -ImagePath $image | Get-Disk | Get-Partition | Where-Object DriveLetter -eq $volume.DriveLetter
    if(-not $resolved){throw '시험 드라이브와 이미지 연결 확인 실패'}
    if($FileManagement){
        # 검증된 새 시험 볼륨에만 일반 사용자 파일 작업용 권한 설정
        $fixtureAcl=Get-Acl -LiteralPath $root
        $fixtureUser=[Security.Principal.WindowsIdentity]::GetCurrent().User
        $fixtureRule=[Security.AccessControl.FileSystemAccessRule]::new($fixtureUser,'FullControl','ContainerInherit,ObjectInherit','None','Allow')
        $fixtureAcl.AddAccessRule($fixtureRule)
        Set-Acl -LiteralPath $root -AclObject $fixtureAcl
    }
    [void][IO.Directory]::CreateDirectory((Join-Path $root 'Cases/sub'))
    [IO.File]::WriteAllText((Join-Path $root ('.diskweave-validation-'+$token)),$token,[Text.UTF8Encoding]::new($false))
    [IO.File]::WriteAllBytes((Join-Path $root 'Cases/regular.bin'),[byte[]](1..13))
    New-Item -ItemType HardLink -Path (Join-Path $root 'Cases/sub/alias.bin') -Target (Join-Path $root 'Cases/regular.bin') | Out-Null
    if($Layout -or $Mft){
        [void][IO.Directory]::CreateDirectory((Join-Path $root 'Outside'))
        New-Item -ItemType HardLink -Path (Join-Path $root 'Outside/alias.bin') -Target (Join-Path $root 'Cases/regular.bin') | Out-Null
    }
    [IO.File]::WriteAllBytes((Join-Path $root 'Cases/empty'),[byte[]]@())
    $sparse=Join-Path $root 'Cases/sparse.bin'
    [IO.File]::WriteAllBytes($sparse,[byte[]]@())
    & fsutil.exe sparse setflag $sparse | Out-File (Join-Path $runDir 'sparse.log')
    if($LASTEXITCODE -ne 0){throw '시험 Sparse 설정 실패'}
    $stream=[IO.File]::OpenWrite($sparse);try{$stream.SetLength(4194304)}finally{$stream.Dispose()}
    [IO.File]::WriteAllBytes((Join-Path $root 'Cases/compressed.bin'),[byte[]]::new(65536))
    & compact.exe /c /i /q (Join-Path $root 'Cases/compressed.bin') | Out-File (Join-Path $runDir 'compression.log')
    if($LASTEXITCODE -ne 0){throw '시험 압축 설정 실패'}
    Set-Content -LiteralPath (Join-Path $root 'Cases/regular.bin') -Stream extra -Value 'ADS excluded' -Encoding UTF8
    New-Item -ItemType Junction -Path (Join-Path $root 'Cases/loop') -Target (Join-Path $root 'Cases') | Out-Null
    [void][IO.Directory]::CreateDirectory((Join-Path $root 'Cases/bulk'))
    for($i=0;$i -lt $FileCount;$i++){[IO.File]::WriteAllBytes((Join-Path $root ('Cases/bulk/entry-{0:D5}.bin' -f $i)),[byte[]](1..8))}
    $guid=(Get-Volume -DriveLetter $volume.DriveLetter).Path
    $request=Join-Path $runDir 'request.json'
    [IO.File]::WriteAllText($request,(@{root=$root;volume=$guid;token=$token;file_count=$FileCount;usn=$Usn.IsPresent;file_management=$FileManagement.IsPresent;layout=$Layout.IsPresent;mft=$Mft.IsPresent;product_mft=$ProductMft.IsPresent}|ConvertTo-Json),[Text.UTF8Encoding]::new($false))
    if($WaitForClient){
        if(-not $FileManagement){throw '별도 클라이언트 대기는 파일 관리 시험에 한정함'}
        [IO.File]::WriteAllText((Join-Path $runDir 'client-ready.txt'),$request,[Text.UTF8Encoding]::new($false))
        $clientStart=Get-Date
        while(-not (Test-Path -LiteralPath (Join-Path $runDir 'client-done.txt'))){
            if((Get-Date)-$clientStart -gt [TimeSpan]::FromMinutes(10)){throw '파일 관리 클라이언트 대기 시간 초과'}
            Start-Sleep -Milliseconds 100
        }
        if([IO.File]::ReadAllText((Join-Path $runDir 'client-done.txt')).Trim() -ne '0'){throw '일반 권한 파일 관리 대조 실패'}
        Get-Content -LiteralPath (Join-Path $runDir 'file-results.json') -Encoding UTF8
        return
    }
    $trialStart=Get-Date
    $trial=Start-Process -FilePath $Runner -ArgumentList @('"'+$request+'"') -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $runDir 'result.json') -RedirectStandardError (Join-Path $runDir 'trace.log')
    $ownedProcessHandle=$trial.Handle
    $peakCombined=0L
    $processPeaks=@{}
    $unavailableSamples=0
    while(-not $trial.HasExited){
        $combined=0L
        foreach($process in @(Get-Process -Name 'diskweave-ntfs-validation' -ErrorAction SilentlyContinue)){
            try { if($process.StartTime -ge $trialStart -and $process.Path -eq $Runner){
                $combined+=$process.WorkingSet64
                $processPeaks[[string]$process.Id]=[Math]::Max([long]$processPeaks[[string]$process.Id],$process.PeakWorkingSet64)
            }}catch [System.InvalidOperationException]{$unavailableSamples++}catch [System.ComponentModel.Win32Exception]{$unavailableSamples++}
        }
        $peakCombined=[Math]::Max($peakCombined,$combined)
        if((Get-Date)-$trialStart -gt [TimeSpan]::FromMinutes(5)){Stop-Process -Id $trial.Id;throw '시험 실행 시간 초과'}
        Start-Sleep -Milliseconds 25
        $trial.Refresh()
    }
    $trial.WaitForExit()
    [IO.File]::WriteAllText((Join-Path $runDir 'memory.json'),(@{sample_interval_ms=25;unavailable_samples=$unavailableSamples;sampled_peak_combined_working_set=$peakCombined;observed_process_peak_working_sets=$processPeaks;note='검증 실행기와 같은 경로에서 이번에 생성된 도우미의 작업 집합 표본이며 제품 GUI 메모리 측정과 구분함'}|ConvertTo-Json),[Text.UTF8Encoding]::new($false))
    if($trial.ExitCode -ne 0){Get-Content (Join-Path $runDir 'trace.log') -Encoding UTF8 -Tail 30;throw '실제 NTFS 대조 실패'}
    Get-Content (Join-Path $runDir 'result.json') -Encoding UTF8
} finally {
    if($mounted){Dismount-DiskImage -ImagePath $image}
    Write-Output ('시험 기록: '+$runDir)
}
